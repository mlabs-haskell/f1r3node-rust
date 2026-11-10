use clap::{Parser, Subcommand};
use rand::RngCore;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use identity_registry::IdentityRegistryClient;
use identity_registry::payload::{NAME_LEN, NONCE_LEN};
use std::env;

const DEFAULT_NODE_HOST: &str = "127.0.0.1";
const DEFAULT_GRPC_PORT: u16 = 40422;

#[derive(Parser)]
#[command(name = "registry-cli", about = "Identity registry CLI")]
struct Cli {
    /// URI of the identity registry contract. Defaults to the registry
    /// deployed at genesis.
    #[arg(long, env = "IDENTITY_REGISTRY_URI", default_value = "rho:system:identityRegistry")]
    registry_uri: Option<String>,

    /// Host of the F1R3FLY validator node.
    #[arg(long, default_value = DEFAULT_NODE_HOST)]
    node_host: String,

    /// gRPC port on the F1R3FLY validator node.
    #[arg(long, default_value_t = DEFAULT_GRPC_PORT)]
    grpc_port: u16,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register a new identity. Prints the derived name as hex.
    Register {
        /// Identity secret key (32-byte hex). Generated fresh if omitted.
        #[arg(long)]
        sk: Option<String>,

        /// Nonce (32-byte hex). Generated randomly if omitted.
        #[arg(long)]
        nonce: Option<String>,
    },
    /// Rotate an identity's current signing key.
    Rotate {
        /// Identity name (32-byte hex).
        #[arg(long)]
        name: String,

        /// Current secret key (32-byte hex). Must sign the rotate payload.
        #[arg(long)]
        current_sk: String,

        /// New secret key (32-byte hex). Must also sign the rotate payload
        /// as proof the submitter holds the new key.
        #[arg(long)]
        new_sk: String,

        /// Expected current seq.
        #[arg(long)]
        seq: u64,
    },
    /// Admin-relink an identity when its current key is compromised. The
    /// signer must be the registry admin (the genesis
    /// identity-registry-admin-public-key). Reads F1R3FLY_SIGNING_KEY.
    AdminRelink {
        /// Identity name (32-byte hex).
        #[arg(long)]
        name: String,

        /// New secret key (32-byte hex). Must sign the relink payload as
        /// proof the new key holder consents to taking over the identity.
        #[arg(long)]
        new_sk: String,

        /// Expected current seq.
        #[arg(long)]
        seq: u64,

        /// Hex-encoded admin evidence recorded verbatim in history.
        #[arg(long)]
        evidence: String,
    },
    /// Resolve the current signing key and seq.
    ResolveCurrent {
        /// Identity name (32-byte hex).
        #[arg(long)]
        name: String,
    },
    /// Resolve the full key history.
    ResolveHistory {
        /// Identity name (32-byte hex).
        #[arg(long)]
        name: String,
    },
    /// Generate a fresh secp256k1 keypair without touching the registry.
    /// Prints the secret key (32-byte hex) and the uncompressed public key
    /// (65-byte hex).
    Keygen,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let deploy_sk_hex = env::var("F1R3FLY_SIGNING_KEY")
        .map_err(|_| "F1R3FLY_SIGNING_KEY must be set")?;

    let client = IdentityRegistryClient::new(
        &deploy_sk_hex,
        &cli.node_host,
        cli.grpc_port,
        cli.registry_uri.clone(),
    )?;

    match cli.command {
        Command::Register { sk, nonce } => {
            let sk_bytes = match sk {
                Some(s) => parse_hex_array::<32>(&s, "sk")?,
                None => random_bytes::<32>(),
            };
            let identity_sk = SecretKey::from_byte_array(sk_bytes)?;
            let nonce_bytes = match nonce {
                Some(s) => parse_hex_array::<NONCE_LEN>(&s, "nonce")?,
                None => random_bytes::<NONCE_LEN>(),
            };

            let identity_pk = PublicKey::from_secret_key(&Secp256k1::new(), &identity_sk);
            let name = client.register(&identity_sk, &nonce_bytes).await?;
            println!("identity_sk (keep safe): {}", hex::encode(sk_bytes));
            println!("identity_pk:             {}", hex::encode(identity_pk.serialize_uncompressed()));
            println!("nonce:                   {}", hex::encode(nonce_bytes));
            println!("name:                    {}", hex::encode(name));
        }
        Command::Rotate { name, current_sk, new_sk, seq } => {
            let name_bytes = parse_hex_array::<NAME_LEN>(&name, "name")?;
            let current_sk_bytes = parse_hex_array::<32>(&current_sk, "current_sk")?;
            let new_sk_bytes = parse_hex_array::<32>(&new_sk, "new_sk")?;
            let current = SecretKey::from_byte_array(current_sk_bytes)?;
            let new = SecretKey::from_byte_array(new_sk_bytes)?;
            let new_seq = client.rotate(&current, &name_bytes, &new, seq).await?;
            println!("new_seq: {new_seq}");
        }
        Command::AdminRelink { name, new_sk, seq, evidence } => {
            let name_bytes = parse_hex_array::<NAME_LEN>(&name, "name")?;
            let new_sk_bytes = parse_hex_array::<32>(&new_sk, "new_sk")?;
            let evidence_bytes = hex::decode(&evidence)?;
            let admin_sk_bytes = parse_hex_array::<32>(&deploy_sk_hex, "F1R3FLY_SIGNING_KEY")?;
            let admin_sk = SecretKey::from_byte_array(admin_sk_bytes)?;
            let new = SecretKey::from_byte_array(new_sk_bytes)?;
            let new_seq = client
                .admin_relink(&admin_sk, &name_bytes, &new, seq, &evidence_bytes)
                .await?;
            println!("new_seq: {new_seq}");
        }
        Command::ResolveCurrent { name } => {
            let name_bytes = parse_hex_array::<NAME_LEN>(&name, "name")?;
            let resolved = client.resolve_current(&name_bytes).await?;
            println!("pk:  {}", hex::encode(resolved.pk));
            println!("seq: {}", resolved.seq);
        }
        Command::ResolveHistory { name } => {
            let name_bytes = parse_hex_array::<NAME_LEN>(&name, "name")?;
            let history = client.resolve_history(&name_bytes).await?;
            for (i, entry) in history.iter().enumerate() {
                println!("entry {i}:");
                println!("  op:  {}", entry.op);
                println!("  seq: {}", entry.seq);
                println!("  pk:  {}", hex::encode(entry.pk));
                if let Some(ev) = &entry.admin_evidence {
                    println!("  admin_evidence: {}", hex::encode(ev));
                }
            }
        }
        Command::Keygen => {
            let sk_bytes = random_bytes::<32>();
            let sk = SecretKey::from_byte_array(sk_bytes)?;
            let pk = PublicKey::from_secret_key(&Secp256k1::new(), &sk);
            println!("sk: {}", hex::encode(sk_bytes));
            println!("pk: {}", hex::encode(pk.serialize_uncompressed()));
        }
    }

    Ok(())
}

fn parse_hex_array<const N: usize>(s: &str, label: &str) -> Result<[u8; N], String> {
    let bytes = hex::decode(s).map_err(|e| format!("{label} is not valid hex: {e}"))?;
    if bytes.len() != N {
        return Err(format!(
            "{label} must be {N} bytes ({} hex chars), got {}",
            N * 2,
            bytes.len()
        ));
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rng().fill_bytes(&mut buf);
    buf
}
