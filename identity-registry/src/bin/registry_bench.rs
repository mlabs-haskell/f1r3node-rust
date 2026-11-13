// Throughput benchmark for the identity registry. Submits deploys at a fixed
// rate without proposing, so the validator's heartbeat decides the blocks.
// Each deploy is tracked by its deploy ID until the node reports a final
// state, and its reply is read at the block that holds it.

use clap::{Parser, ValueEnum};
use identity_registry::client::decode_resolve_current;
use identity_registry::payload::{NAME_LEN, NONCE_LEN, derive_name};
use identity_registry::reply::{self, DeployClient, extract_gbytearray, extract_gint, parse_tagged_reply};
use identity_registry::signing::uncompressed_pk_bytes;
use identity_registry::term::new_reply_slot;
use identity_registry::{IdentityRegistryClient, IdentityRegistryError};
use models::casper::DeployFinalizationStateProto;
use models::rhoapi::Par;
use rand::{Rng, RngCore};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, ValueEnum)]
enum Scenario {
    Register,
    Mixed,
    /// A deploy that only writes its reply slot. Baseline for node cost
    /// without the registry.
    Noop,
}

#[derive(Parser)]
#[command(name = "registry-bench", about = "Identity registry throughput benchmark")]
struct Cli {
    #[arg(long, env = "IDENTITY_REGISTRY_URI")]
    registry_uri: String,

    #[arg(long, default_value = "127.0.0.1")]
    node_host: String,

    /// gRPC port of the validator that receives the deploys.
    #[arg(long, default_value_t = 40422)]
    grpc_port: u16,

    /// gRPC port for status polls and reply reads. Defaults to --grpc-port.
    /// Point it at the observer (40452) to keep poll load off the validator.
    #[arg(long)]
    query_port: Option<u16>,

    #[arg(long, value_enum)]
    scenario: Scenario,

    /// Target submit rate, deploys per second.
    #[arg(long, default_value_t = 1.0)]
    rate: f64,

    /// Length of the submit phase, seconds.
    #[arg(long, default_value_t = 60)]
    duration: u64,

    /// Send exactly this many deploys in one burst, as fast as possible.
    /// Overrides --rate and --duration.
    #[arg(long)]
    count: Option<usize>,

    /// Time to wait for outstanding deploys after the submit phase, seconds.
    #[arg(long, default_value_t = 300)]
    drain: u64,

    /// Mixed scenario weights: register,rotate,resolve.
    #[arg(long, default_value = "10,30,60")]
    mix: String,

    /// Identity state file, one `name_hex sk_hex seq` per line. Read at the
    /// start if present and rewritten at the end. Run the register scenario
    /// first to prefill it for the mixed scenario.
    #[arg(long, default_value = "bench-state.txt")]
    state: PathBuf,

    /// Per-op CSV output.
    #[arg(long)]
    csv: Option<PathBuf>,

    /// Interval between status polls of one deploy, milliseconds.
    #[arg(long, default_value_t = 1000)]
    poll_ms: u64,

    /// After the run, resolve every identity this run changed and compare
    /// against the expected key and seq.
    #[arg(long)]
    check: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Register,
    Rotate,
    Resolve,
    Noop,
}

struct Identity {
    name: [u8; NAME_LEN],
    sk: SecretKey,
    seq: u64,
    /// A rotate on this identity is outstanding.
    busy: bool,
    /// This run changed the identity.
    touched: bool,
    /// A rotate ended without a known result, so the seq is unknown.
    unknown: bool,
}

enum Planned {
    Register { sk: SecretKey, nonce: [u8; NONCE_LEN] },
    Rotate { idx: usize, new_sk: SecretKey },
    Resolve { idx: usize },
    Check { expected_pk: [u8; 65], expected_seq: u64 },
    Noop,
}

struct Outcome {
    kind: Kind,
    deploy_id: String,
    block: Option<String>,
    submit_at: Duration,
    included_after: Option<Duration>,
    finalized_after: Option<Duration>,
    rejections: u32,
    result: String,
}

struct Ctx {
    client: IdentityRegistryClient,
    query: DeployClient,
    poll: Duration,
    start: Instant,
}

type State = Arc<Mutex<Vec<Identity>>>;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let deploy_sk_hex =
        env::var("F1R3FLY_SIGNING_KEY").map_err(|_| "F1R3FLY_SIGNING_KEY must be set")?;
    let weights = parse_mix(&cli.mix)?;
    if cli.rate <= 0.0 {
        return Err("--rate must be positive".into());
    }

    let client = IdentityRegistryClient::new(
        &deploy_sk_hex,
        &cli.node_host,
        cli.grpc_port,
        Some(cli.registry_uri.clone()),
    )?;
    let query = reply::connect(&cli.node_host, cli.query_port.unwrap_or(cli.grpc_port)).await?;
    let ctx = Arc::new(Ctx {
        client,
        query,
        poll: Duration::from_millis(cli.poll_ms),
        start: Instant::now(),
    });
    let state: State = Arc::new(Mutex::new(load_state(&cli.state)?));
    let prefilled = state.lock().unwrap().len();

    let duration = Duration::from_secs(if cli.count.is_some() { 0 } else { cli.duration });
    let deadline = ctx.start + duration + Duration::from_secs(cli.drain);
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / cli.rate));
    let mut handles = Vec::new();
    loop {
        match cli.count {
            Some(n) if handles.len() >= n => break,
            Some(_) => {}
            None if ctx.start.elapsed() >= duration => break,
            None => {
                ticker.tick().await;
            }
        }
        let planned = match cli.scenario {
            Scenario::Register => plan_register(),
            Scenario::Mixed => plan_mixed(&state, &weights),
            Scenario::Noop => Planned::Noop,
        };
        handles.push(spawn_op(&ctx, &state, planned, deadline)?);
    }
    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await?);
    }

    let blocks = fetch_blocks(&ctx, &outcomes).await;
    summarize(&cli, prefilled, &outcomes, &blocks);
    if let Some(path) = &cli.csv {
        write_csv(path, &outcomes, &blocks)?;
    }

    if cli.check {
        check(&ctx, &state, cli.rate, Duration::from_secs(cli.drain)).await?;
    }
    save_state(&cli.state, &state.lock().unwrap())?;
    Ok(())
}

fn parse_mix(s: &str) -> Result<[u32; 3], String> {
    let parts: Vec<u32> = s
        .split(',')
        .map(|p| p.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("--mix: {e}"))?;
    match parts[..] {
        [a, b, c] if a + b + c > 0 => Ok([a, b, c]),
        _ => Err("--mix needs three weights register,rotate,resolve with a positive sum".into()),
    }
}

fn plan_register() -> Planned {
    let (sk, _) = Secp256k1::new().generate_keypair(&mut rand::rng());
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    Planned::Register { sk, nonce }
}

fn plan_mixed(state: &State, weights: &[u32; 3]) -> Planned {
    let mut rng = rand::rng();
    let ids = state.lock().unwrap();
    if ids.is_empty() {
        return plan_register();
    }
    let roll = rng.random_range(0..weights.iter().sum::<u32>());
    if roll < weights[0] {
        return plan_register();
    }
    if roll < weights[0] + weights[1] {
        // ponytail: random probing for a free identity, fine while most are idle
        for _ in 0..16 {
            let idx = rng.random_range(0..ids.len());
            if !ids[idx].busy && !ids[idx].unknown {
                let (new_sk, _) = Secp256k1::new().generate_keypair(&mut rng);
                return Planned::Rotate { idx, new_sk };
            }
        }
    }
    Planned::Resolve { idx: rng.random_range(0..ids.len()) }
}

fn spawn_op(
    ctx: &Arc<Ctx>,
    state: &State,
    planned: Planned,
    deadline: Instant,
) -> Result<tokio::task::JoinHandle<Outcome>, IdentityRegistryError> {
    let (kind, (term, slot)) = {
        let mut ids = state.lock().unwrap();
        match &planned {
            Planned::Register { sk, nonce } => (Kind::Register, ctx.client.prepare_register(sk, nonce)?),
            Planned::Rotate { idx, new_sk } => {
                let id = &mut ids[*idx];
                id.busy = true;
                (Kind::Rotate, ctx.client.prepare_rotate(&id.sk, &id.name, new_sk, id.seq)?)
            }
            Planned::Resolve { idx } => {
                (Kind::Resolve, ctx.client.prepare_resolve_current(&ids[*idx].name)?)
            }
            Planned::Noop => {
                let slot = new_reply_slot();
                (Kind::Noop, (format!(r#"@"{slot}"!(("ok", 0))"#), slot))
            }
            Planned::Check { .. } => unreachable!("check ops are built in check()"),
        }
    };
    let ctx = ctx.clone();
    let state = state.clone();
    Ok(tokio::spawn(async move {
        let (out, value) = track(&ctx, kind, &term, &slot, deadline).await;
        apply(&state, planned, out, value)
    }))
}

/// Submits one deploy and follows it to a final state. Returns the decoded
/// `ok` value of the reply, if any.
async fn track(
    ctx: &Ctx,
    kind: Kind,
    term: &str,
    slot: &str,
    deadline: Instant,
) -> (Outcome, Option<Par>) {
    let submitted = Instant::now();
    let mut out = Outcome {
        kind,
        deploy_id: String::new(),
        block: None,
        submit_at: submitted - ctx.start,
        included_after: None,
        finalized_after: None,
        rejections: 0,
        result: String::new(),
    };
    match ctx.client.submit(term).await {
        Ok(id) => out.deploy_id = id,
        Err(e) => {
            eprintln!("submit failed: {e}");
            out.result = "submit_error".into();
            return (out, None);
        }
    }

    let mut q = ctx.query.clone();
    loop {
        tokio::time::sleep(ctx.poll).await;
        if Instant::now() > deadline {
            out.result = "pending".into();
            return (out, None);
        }
        let Ok(status) = reply::deploy_status(&mut q, &out.deploy_id).await else {
            continue;
        };
        out.rejections = status.rejection_count;
        if let Some(hash) = &status.latest_block_hash {
            out.block = Some(hex::encode(hash));
            out.included_after.get_or_insert(submitted.elapsed());
        }
        match DeployFinalizationStateProto::try_from(status.state) {
            Ok(DeployFinalizationStateProto::DeployStateFinalized) => break,
            Ok(DeployFinalizationStateProto::DeployStateFailed) => {
                out.result = "failed".into();
                return (out, None);
            }
            Ok(DeployFinalizationStateProto::DeployStateExpired) => {
                out.result = "expired".into();
                return (out, None);
            }
            _ => {}
        }
    }
    out.finalized_after = Some(submitted.elapsed());

    let Some(block) = out.block.clone() else {
        out.result = "no_block".into();
        return (out, None);
    };
    match reply::fetch_reply_with(&mut q, slot, &block)
        .await
        .and_then(parse_tagged_reply)
    {
        Ok(value) => {
            out.result = "ok".into();
            (out, Some(value))
        }
        Err(IdentityRegistryError::Rejected(reason)) => {
            out.result = format!("err:{reason}");
            (out, None)
        }
        Err(_) => {
            out.result = "no_reply".into();
            (out, None)
        }
    }
}

/// Updates the identity state from one finished op, and checks the reply
/// against what the op expects.
fn apply(state: &State, planned: Planned, mut out: Outcome, value: Option<Par>) -> Outcome {
    let mut ids = state.lock().unwrap();
    match planned {
        Planned::Register { sk, nonce } => {
            if let Some(v) = value {
                let pk = uncompressed_pk_bytes(&PublicKey::from_secret_key(&Secp256k1::new(), &sk));
                let name = derive_name(&pk, &nonce);
                if extract_gbytearray(&v).ok().as_deref() == Some(&name[..]) {
                    ids.push(Identity { name, sk, seq: 0, busy: false, touched: true, unknown: false });
                } else {
                    out.result = "bad_reply".into();
                }
            }
        }
        Planned::Rotate { idx, new_sk } => {
            let id = &mut ids[idx];
            id.busy = false;
            match (value.as_ref().map(extract_gint), out.result.as_str()) {
                (Some(Ok(new_seq)), _) => {
                    id.sk = new_sk;
                    id.seq = new_seq as u64;
                    id.touched = true;
                }
                (Some(Err(_)), _) => out.result = "bad_reply".into(),
                (None, "pending" | "no_reply" | "no_block") => id.unknown = true,
                (None, _) => {}
            }
        }
        Planned::Resolve { .. } | Planned::Noop => {}
        Planned::Check { expected_pk, expected_seq } => {
            if let Some(v) = value {
                match decode_resolve_current(v) {
                    Ok(r) if r.pk == expected_pk && r.seq == expected_seq => {}
                    _ => out.result = "mismatch".into(),
                }
            }
        }
    }
    out
}

/// Resolves every identity this run changed and compares the chain against
/// the expected key and seq.
async fn check(
    ctx: &Arc<Ctx>,
    state: &State,
    rate: f64,
    drain: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let targets: Vec<([u8; NAME_LEN], [u8; 65], u64)> = state
        .lock()
        .unwrap()
        .iter()
        .filter(|id| id.touched && !id.unknown)
        .map(|id| {
            let pk = PublicKey::from_secret_key(&Secp256k1::new(), &id.sk);
            (id.name, uncompressed_pk_bytes(&pk), id.seq)
        })
        .collect();
    println!("check: resolving {} identities", targets.len());

    let deadline = Instant::now() + Duration::from_secs_f64(targets.len() as f64 / rate) + drain;
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
    let mut handles = Vec::new();
    for (name, expected_pk, expected_seq) in targets {
        ticker.tick().await;
        let (term, slot) = ctx.client.prepare_resolve_current(&name)?;
        let ctx = ctx.clone();
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            let (out, value) = track(&ctx, Kind::Resolve, &term, &slot, deadline).await;
            let out = apply(&state, Planned::Check { expected_pk, expected_seq }, out, value);
            (name, out.result)
        }));
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for h in handles {
        let (name, result) = h.await?;
        if result != "ok" {
            println!("check: {} {}", hex::encode(name), result);
        }
        *counts.entry(result).or_default() += 1;
    }
    println!("check: {counts:?}");
    Ok(())
}

/// Block hash -> (timestamp ms, deploy count, cost by deploy ID).
type Blocks = HashMap<String, (i64, usize, HashMap<String, u64>)>;

async fn fetch_blocks(ctx: &Ctx, outcomes: &[Outcome]) -> Blocks {
    let mut q = ctx.query.clone();
    let mut blocks = Blocks::new();
    for hash in outcomes.iter().filter_map(|o| o.block.as_ref()) {
        if blocks.contains_key(hash) {
            continue;
        }
        match reply::block_info(&mut q, hash).await {
            Ok(info) => {
                let ts = info.block_info.as_ref().map_or(0, |b| b.timestamp);
                let costs = info.deploys.iter().map(|d| (d.sig.clone(), d.cost)).collect();
                blocks.insert(hash.clone(), (ts, info.deploys.len(), costs));
            }
            Err(e) => eprintln!("get_block {hash} failed: {e}"),
        }
    }
    blocks
}

fn cost_of(o: &Outcome, blocks: &Blocks) -> Option<u64> {
    blocks.get(o.block.as_ref()?)?.2.get(&o.deploy_id).copied()
}

fn summarize(cli: &Cli, prefilled: usize, outcomes: &[Outcome], blocks: &Blocks) {
    let scenario = match cli.scenario {
        Scenario::Register => "register",
        Scenario::Mixed => "mixed",
        Scenario::Noop => "noop",
    };
    let window = cli.duration as f64;
    println!("== registry bench: per-node ceiling, all deploys enter through port {}", cli.grpc_port);
    println!(
        "scenario={scenario} mix={} rate={}/s duration={}s prefilled={prefilled} submitted={}",
        cli.mix,
        cli.rate,
        cli.duration,
        outcomes.len()
    );

    let mut results: BTreeMap<&str, usize> = BTreeMap::new();
    for o in outcomes {
        *results.entry(o.result.as_str()).or_default() += 1;
    }
    println!("outcomes: {results:?}");
    println!(
        "merge-rejected at least once: {}",
        outcomes.iter().filter(|o| o.rejections > 0).count()
    );

    let ok: Vec<&Outcome> = outcomes.iter().filter(|o| o.result == "ok").collect();
    let included_in_window = outcomes
        .iter()
        .filter(|o| o.included_after.is_some_and(|d| (o.submit_at + d).as_secs_f64() <= window))
        .count();
    let last_ok = ok
        .iter()
        .filter_map(|o| o.finalized_after.map(|d| (o.submit_at + d).as_secs_f64()))
        .fold(0.0, f64::max);
    println!(
        "throughput: included during submit phase {:.2}/s, finalized ok {:.2}/s ({} ok in {:.1}s)",
        included_in_window as f64 / window,
        if last_ok > 0.0 { ok.len() as f64 / last_ok } else { 0.0 },
        ok.len(),
        last_ok
    );

    let ms = |f: fn(&Outcome) -> Option<Duration>| -> Vec<f64> {
        ok.iter().filter_map(|o| f(o)).map(|d| d.as_secs_f64() * 1000.0).collect()
    };
    println!("latency submit->included (ms, ok ops): {}", percentiles(ms(|o| o.included_after)));
    println!("latency submit->finalized (ms, ok ops): {}", percentiles(ms(|o| o.finalized_after)));

    for kind in [Kind::Register, Kind::Rotate, Kind::Resolve, Kind::Noop] {
        let of_kind: Vec<&Outcome> = outcomes.iter().filter(|o| o.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        let costs: Vec<u64> = of_kind.iter().filter_map(|o| cost_of(o, blocks)).collect();
        let mean_cost = if costs.is_empty() { 0 } else { costs.iter().sum::<u64>() / costs.len() as u64 };
        println!(
            "{kind:?}: submitted={} ok={} mean_phlo={mean_cost}",
            of_kind.len(),
            of_kind.iter().filter(|o| o.result == "ok").count()
        );
    }

    let mut ours_per_block: HashMap<&str, usize> = HashMap::new();
    for o in outcomes {
        if let Some(b) = &o.block {
            *ours_per_block.entry(b.as_str()).or_default() += 1;
        }
    }
    if !ours_per_block.is_empty() {
        let n = ours_per_block.len() as f64;
        let mut ts: Vec<i64> = blocks.values().map(|b| b.0).collect();
        ts.sort();
        let interval = if ts.len() > 1 {
            (ts[ts.len() - 1] - ts[0]) as f64 / (ts.len() - 1) as f64 / 1000.0
        } else {
            0.0
        };
        println!(
            "blocks: {} carry our deploys, ours/block mean {:.1} max {}, all deploys/block mean {:.1}, mean interval {:.2}s",
            ours_per_block.len(),
            ours_per_block.values().sum::<usize>() as f64 / n,
            ours_per_block.values().max().unwrap_or(&0),
            blocks.values().map(|b| b.1).sum::<usize>() as f64 / blocks.len().max(1) as f64,
            interval
        );
    }
}

fn percentiles(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "n/a".into();
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!("p50={:.0} p95={:.0} p99={:.0} max={:.0}", at(0.5), at(0.95), at(0.99), at(1.0))
}

fn write_csv(path: &Path, outcomes: &[Outcome], blocks: &Blocks) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(f, "kind,deploy_id,block,submit_ms,included_ms,finalized_ms,rejections,cost,result")?;
    let ms = |d: Option<Duration>| d.map(|d| d.as_millis().to_string()).unwrap_or_default();
    for o in outcomes {
        writeln!(
            f,
            "{:?},{},{},{},{},{},{},{},{}",
            o.kind,
            o.deploy_id,
            o.block.as_deref().unwrap_or(""),
            o.submit_at.as_millis(),
            ms(o.included_after),
            ms(o.finalized_after),
            o.rejections,
            cost_of(o, blocks).map(|c| c.to_string()).unwrap_or_default(),
            o.result
        )?;
    }
    Ok(())
}

fn load_state(path: &Path) -> Result<Vec<Identity>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for line in fs::read_to_string(path)?.lines().filter(|l| !l.trim().is_empty()) {
        let [name, sk, seq] = line.split_whitespace().collect::<Vec<_>>()[..] else {
            return Err(format!("bad state line: {line}").into());
        };
        let name: [u8; NAME_LEN] = hex::decode(name)?.try_into().map_err(|_| "bad name length")?;
        let sk: [u8; 32] = hex::decode(sk)?.try_into().map_err(|_| "bad sk length")?;
        ids.push(Identity {
            name,
            sk: SecretKey::from_byte_array(sk)?,
            seq: seq.parse()?,
            busy: false,
            touched: false,
            unknown: false,
        });
    }
    Ok(ids)
}

/// Writes every identity whose seq is known. An identity with a rotate of
/// unknown result is left out, because its key on chain is unknown.
fn save_state(path: &Path, ids: &[Identity]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    let dropped = ids.iter().filter(|id| id.unknown).count();
    if dropped > 0 {
        eprintln!("state: dropped {dropped} identities with an unknown rotate result");
    }
    for id in ids.iter().filter(|id| !id.unknown) {
        writeln!(f, "{} {} {}", hex::encode(id.name), hex::encode(id.sk.secret_bytes()), id.seq)?;
    }
    Ok(())
}
