// R0.2 reference probe: exercise the crypto/hash/registry primitives the
// Register relies on from a single deploy against a running standalone node.
//
// Why this is a binary and not `node deploy <file>`: insertSigned:secp256k1
// verifies a secp256k1 signature over blake2b256((deployTimestamp,
// deployerPubKey, version)). The signature must be computed before the deploy
// runs, so the deploy timestamp has to be pinned. The `node deploy` CLI stamps
// SystemTime::now() with no override, so a hand-authored .rho cannot carry a
// matching signature. Here we pick one timestamp T, derive the signature for
// T via RegistrySigGen, and submit a DeployData whose time_stamp == T, signed
// by the same key RegistrySigGen used as the deployer key.
//
// Run: standalone node up (just run-standalone), then
//   STANDALONE_PRIVATE_KEY=<hex> cargo run --release -p casper --example r0_2_primitives_probe

use std::time::{SystemTime, UNIX_EPOCH};

use casper::rust::util::comm::grpc_deploy_service::{DeployService, GrpcDeployService};
use casper::rust::util::comm::grpc_propose_service::{GrpcProposeService, ProposeService};
use casper::rust::util::rholang::registry_sig_gen::{Args, RegistrySigGen};
use crypto::rust::private_key::PrivateKey;
use crypto::rust::signatures::ed25519::Ed25519;
use crypto::rust::signatures::secp256k1::Secp256k1;
use crypto::rust::signatures::signatures_alg::SignaturesAlg;
use crypto::rust::signatures::signed::Signed;
use models::rust::casper::protocol::casper_message::DeployData;

const DEFAULT_SK: &str = "5f668a7ee96d944a4494cc947e4005e172d7ab3461ee5538f1f2a45a835e9657";
const HOST: &str = "127.0.0.1";
const MAX_RECV: usize = 16 * 1024 * 1024;

const TERM: &str = r#"
new
  stdout(`rho:io:stdout`),
  secpVerify(`rho:crypto:secp256k1Verify`),
  edVerify(`rho:crypto:ed25519Verify`),
  sha(`rho:crypto:sha256Hash`),
  keccak(`rho:crypto:keccak256Hash`),
  blake(`rho:crypto:blake2b256Hash`),
  rs(`rho:registry:insertSigned:secp256k1`),
  secpCh, edCh, shaCh, keccakCh, blakeCh, uriOut
in {
  secpVerify!("__SECP_DATA__".hexToBytes(), "__SECP_SIG__".hexToBytes(), "__SECP_PK__".hexToBytes(), *secpCh) |
  for (@ok <- secpCh) { stdout!(("R0.2 secp256k1Verify", ok)) } |

  edVerify!("__ED_DATA__".hexToBytes(), "__ED_SIG__".hexToBytes(), "__ED_PK__".hexToBytes(), *edCh) |
  for (@ok <- edCh) { stdout!(("R0.2 ed25519Verify", ok)) } |

  sha!("R0.2 hash family".toByteArray(), *shaCh) |
  for (@h <- shaCh) { stdout!(("R0.2 sha256Hash", h)) } |

  keccak!("R0.2 hash family".toByteArray(), *keccakCh) |
  for (@h <- keccakCh) { stdout!(("R0.2 keccak256Hash", h)) } |

  blake!("R0.2 hash family".toByteArray(), *blakeCh) |
  for (@h <- blakeCh) { stdout!(("R0.2 blake2b256Hash", h)) } |

  rs!("__REG_PK__".hexToBytes(), (__REG_NONCE__, "R0.2-insertSigned-probe"), "__REG_SIG__".hexToBytes(), *uriOut) |
  for (@uri <- uriOut) { stdout!(("R0.2 insertSigned uri", uri)) }
}
"#;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let sk_hex = std::env::var("STANDALONE_PRIVATE_KEY").unwrap_or_else(|_| DEFAULT_SK.to_string());
    let deployer_sk = PrivateKey::from_bytes(&hex::decode(sk_hex.trim())?);

    // Single pinned timestamp, reused for the registry signature and the deploy.
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    // --- insertSigned signature (bound to timestamp + deployer key) ---
    let args = Args::new(
        Some("Probe".to_string()),
        Some(timestamp),
        Some(deployer_sk.clone()),
    );
    let derivation = RegistrySigGen::derive_from(&args);
    let reg_pk = derivation.result.pk.to_string();
    let reg_sig = derivation.result.sig.to_string();
    let reg_nonce = derivation.result.nonce;

    // --- secp256k1Verify vector (data is a 32-byte prehash) ---
    let secp = Secp256k1;
    let (secp_sk, secp_pk) = secp.new_key_pair();
    let secp_data: Vec<u8> = (0u8..32).collect();
    let secp_sig = secp.sign(&secp_data, &secp_sk.bytes);

    // --- ed25519Verify vector (signs the raw message) ---
    let ed = Ed25519;
    let (ed_sk, ed_pk) = ed.new_key_pair();
    let ed_data = b"R0.2 ed25519 probe".to_vec();
    let ed_sig = ed.sign(&ed_data, &ed_sk.bytes);

    let term = TERM
        .replace("__SECP_DATA__", &hex::encode(&secp_data))
        .replace("__SECP_SIG__", &hex::encode(&secp_sig))
        .replace("__SECP_PK__", &hex::encode(&secp_pk.bytes))
        .replace("__ED_DATA__", &hex::encode(&ed_data))
        .replace("__ED_SIG__", &hex::encode(&ed_sig))
        .replace("__ED_PK__", &hex::encode(&ed_pk.bytes))
        .replace("__REG_PK__", &reg_pk)
        .replace("__REG_SIG__", &reg_sig)
        .replace("__REG_NONCE__", &reg_nonce.to_string());

    let data = DeployData {
        term,
        time_stamp: timestamp,
        phlo_price: 1,
        phlo_limit: 500_000_000,
        valid_after_block_number: 0,
        shard_id: "root".to_string(),
        expiration_timestamp: None,
        parameters: Vec::new(),
    };
    let signed = Signed::create(data, Box::new(Secp256k1), deployer_sk)
        .map_err(|e| eyre::eyre!("sign deploy: {e}"))?;

    // --- submit + propose ---
    let mut deploy_svc = GrpcDeployService::new(HOST, 40401, MAX_RECV).await?;
    match deploy_svc.deploy(signed).await {
        Ok(resp) => println!("deploy: {resp}"),
        Err(errs) => return Err(eyre::eyre!("deploy failed: {}", errs.join("; "))),
    }

    let mut propose_svc = GrpcProposeService::new(HOST, 40402, MAX_RECV).await?;
    match propose_svc.propose(false).await {
        Ok(resp) => println!("propose: {resp}"),
        Err(errs) => return Err(eyre::eyre!("propose failed: {}", errs.join("; "))),
    }

    println!("expected insertSigned uri: {}", derivation.uri);
    println!("timestamp: {timestamp}");
    println!("Check the node log for the five `R0.2 ...` stdout lines and the matching uri.");
    Ok(())
}
