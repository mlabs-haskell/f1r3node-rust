#!/usr/bin/env bash
# Measures the cost of one block of N registry deploys on a local shard:
# block creation time, replay time on the other validators, and peak memory
# of every node container. See identity-registry/BENCHMARKS.md.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/block-cost.sh [options]

Resets the local shard (docker/shard.yml), deploys the identity registry, and
for each scenario and size sends one burst of N deploys. Records block
creation time, replay time and peak memory per node container.

Options:
  -s, --scenarios LIST   Space-separated scenarios: noop register resolve
                         (default: "noop register resolve")
  -n, --sizes LIST       Space-separated burst sizes (default: "16 32 64")
  -m, --mem-cap SIZE     Memory cap per node container (default: 4g)
  -o, --out DIR          Output directory
                         (default: target/block-cost/<UTC timestamp>)
  -i, --interval SEC     Memory sample interval (default: 0.2)
      --drain SEC        Max wait for the deploys of one burst (default: 600)
      --no-reset         Use the running shard. Do not wipe it.
  -y, --yes              Do not ask before the shard data is wiped
  -h, --help             Show this help

Environment:
  F1R3FLY_SIGNING_KEY    Deployer key. If unset, BOOTSTRAP_PRIVATE_KEY from
                         docker/.env is used.
EOF
}

die() { echo "error: $*" >&2; exit 1; }
log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
now() { echo "$EPOCHREALTIME"; }
now_ms() { local t=${EPOCHREALTIME/./}; echo $((t / 1000)); }

((BASH_VERSINFO[0] >= 5)) || die "bash 5 or later is required (macOS: brew install bash)"

SCENARIOS="noop register resolve"
SIZES="16 32 64"
MEM_CAP=4g
OUT=""
INTERVAL=0.2
DRAIN=600
RESET=1
YES=0
REPLAY_TIMEOUT=300
SETTLE=5

while (($#)); do
  case $1 in
    -s|--scenarios) SCENARIOS=$2; shift 2 ;;
    -n|--sizes) SIZES=$2; shift 2 ;;
    -m|--mem-cap) MEM_CAP=$2; shift 2 ;;
    -o|--out) OUT=$2; shift 2 ;;
    -i|--interval) INTERVAL=$2; shift 2 ;;
    --drain) DRAIN=$2; shift 2 ;;
    --no-reset) RESET=0; shift ;;
    -y|--yes) YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

for s in $SCENARIOS; do
  [[ $s =~ ^(noop|register|resolve)$ ]] || die "unknown scenario: $s"
done
for n in $SIZES; do
  [[ $n =~ ^[1-9][0-9]*$ ]] || die "bad size: $n"
done

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
COMPOSE=(docker compose --progress quiet -f "$REPO/docker/shard.yml")
NODES=(bootstrap validator1 validator2 validator3 readonly)
REPLAYERS=(validator1 validator3)
CREATOR=validator2
BIN="$REPO/target/debug"
OUT=${OUT:-"$REPO/target/block-cost/$(date -u +%Y%m%dT%H%M%SZ)"}
mkdir -p "$OUT"
STATE="$OUT/state.txt"
MEMFILE="$OUT/memory.tsv"
RESULTS="$OUT/results.csv"

command -v docker >/dev/null || die "docker not found"
command -v cargo >/dev/null || die "cargo not found"
[[ -f $REPO/docker/.env ]] || cp "$REPO/docker/.env.example" "$REPO/docker/.env"
if [[ -z ${F1R3FLY_SIGNING_KEY:-} ]]; then
  F1R3FLY_SIGNING_KEY=$(grep '^BOOTSTRAP_PRIVATE_KEY=' "$REPO/docker/.env" | cut -d= -f2)
  [[ -n $F1R3FLY_SIGNING_KEY ]] || die "F1R3FLY_SIGNING_KEY is unset and docker/.env has no BOOTSTRAP_PRIVATE_KEY"
fi
export F1R3FLY_SIGNING_KEY

SAMPLER_PID=""
cleanup() {
  [[ -n $SAMPLER_PID ]] && kill "$SAMPLER_PID" 2>/dev/null || true
}
trap cleanup EXIT

container_id() { docker inspect -f '{{.Id}}' "rnode.$1"; }
restarts() { docker inspect -f '{{.RestartCount}}' "rnode.$1"; }
all_restarts() { local c; for c in "${NODES[@]}"; do printf '%s ' "$(restarts "$c")"; done; }

cgroup_dir() {
  local id d
  id=$(container_id "$1")
  for d in "/sys/fs/cgroup/system.slice/docker-$id.scope" "/sys/fs/cgroup/docker/$id"; do
    [[ -r $d/memory.stat ]] && { echo "$d"; return 0; }
  done
  return 1
}

# Writes "<ms> <container> <anon_bytes> <current_bytes>" lines to MEMFILE.
sample_cgroup() {
  local -A dirs
  local c k v anon cur t
  while :; do
    for c in "${NODES[@]}"; do
      [[ -n ${dirs[$c]:-} && -r ${dirs[$c]}/memory.stat ]] || dirs[$c]=$(cgroup_dir "$c" 2>/dev/null) || continue
      anon=0
      while read -r k v; do
        [[ $k == anon ]] && { anon=$v; break; }
      done <"${dirs[$c]}/memory.stat" || continue
      read -r cur <"${dirs[$c]}/memory.current" || continue
      t=${EPOCHREALTIME/./}
      echo "$((t / 1000)) $c $anon $cur"
    done
    sleep "$INTERVAL"
  done >>"$MEMFILE"
}

# Fallback without cgroup v2 access: docker stats, about one sample per second.
sample_docker_stats() {
  local t
  while :; do
    t=${EPOCHREALTIME/./}
    docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' "${NODES[@]/#/rnode.}" 2>/dev/null \
      | awk -v t=$((t / 1000)) '{
          v = $2; m = 1
          if (v ~ /GiB$/) m = 1073741824; else if (v ~ /MiB$/) m = 1048576; else if (v ~ /KiB$/) m = 1024
          sub(/[KMG]?i?B$/, "", v); n = $1; sub(/^rnode\./, "", n)
          printf "%d %s %d %d\n", t, n, v * m, v * m
        }' || true
  done >>"$MEMFILE"
}

# Peak anon memory in MiB per container between two ms timestamps.
peaks() {
  awk -v a="$1" -v b="$2" '
    $1 >= a && $1 <= b && $3 > p[$2] { p[$2] = $3 }
    END { for (c in p) printf "%s %d\n", c, p[c] / 1048576 }' "$MEMFILE"
}

# Prints "<block> <hash_prefix> <deploys> <create_ms>" for each block that
# validator2 created with at least one user deploy since <unix_ts>. The node
# logs, in order: deploy selection, propose timing, propose finished.
created_blocks() {
  docker logs --since "$1" "rnode.$CREATOR" 2>&1 \
    | grep -oE 'Deploy selection for block #[0-9]+: [^"]*selected=[0-9]+|Propose timing: [^"]*total_ms=[0-9]+|Block #[0-9]+ \([0-9a-f]+\.\.\.\) created and added' \
    | awk '
        /^Deploy selection/ {
          match($0, /selected=[0-9]+/); k = substr($0, RSTART + 9, RLENGTH - 9) + 0
          ms = ""
          next
        }
        /^Propose timing/ { match($0, /total_ms=[0-9]+/); ms = substr($0, RSTART + 9, RLENGTH - 9); next }
        /created and added/ {
          if (k > 0 && ms != "") {
            match($0, /#[0-9]+/); b = substr($0, RSTART + 1, RLENGTH - 1)
            match($0, /\([0-9a-f]+/); h = substr($0, RSTART + 1, RLENGTH - 1)
            print b, h, k, ms
          }
          k = 0; ms = ""
        }' || true
}

# Replay time in ms of block <hash_prefix> on container <c> since <unix_ts>.
# Empty if the replay has not been logged yet.
replay_ms() {
  docker logs --since "$3" "rnode.$1" 2>&1 \
    | grep -oE "Block replayed: #[0-9]+ \($2\.\.\.\) \([0-9]+d\) \([A-Za-z]+\) \[[0-9.]+(s|ms|µs|ns)\]" \
    | tail -1 \
    | sed -E 's/.*\[([0-9.]+)(s|ms|µs|ns)\]/\1 \2/' \
    | awk '{ m = ($2 == "s") ? 1000 : ($2 == "ms") ? 1 : ($2 == "µs") ? 0.001 : 0.000001; printf "%d\n", $1 * m }' || true
}

wait_for() {
  local timeout=$1 desc=$2; shift 2
  local end=$((SECONDS + timeout))
  until "$@"; do
    ((SECONDS < end)) || die "timed out after ${timeout}s waiting for $desc"
    sleep 3
  done
}

shard_ports_free() {
  local base p
  for base in 40400 40410 40420 40430 40450; do
    for p in $(seq "$base" $((base + 5))); do
      (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null && return 1
    done
  done
  return 0
}

all_nodes_running() {
  local c
  for c in "${NODES[@]}"; do
    [[ $(docker inspect -f '{{.State.Running}}' "rnode.$c" 2>/dev/null) == true ]] || return 1
  done
}

shard_making_blocks() { docker logs "rnode.$CREATOR" 2>&1 | grep -q 'Creating block #'; }

deploy_registry() {
  local attempt out uri
  for attempt in 1 2 3; do
    out=$("$BIN/registry_cli" deploy --contract-path "$REPO/identity-registry/rholang/identity_registry.rho" 2>&1) || true
    uri=$(sed -n 's/^Identity Registry URI: //p' <<<"$out")
    [[ -n $uri ]] && { echo "$uri"; return 0; }
    log "registry deploy attempt $attempt failed: $(tail -1 <<<"$out")"
    sleep 10
  done
  return 1
}

bench_args() {
  case $1 in
    noop) echo "--scenario noop" ;;
    register) echo "--scenario register" ;;
    resolve) echo "--scenario mixed --mix 0,0,100" ;;
  esac
}

# --- Setup ------------------------------------------------------------------

if ((RESET)) && ((!YES)); then
  answer=""
  read -r -p "This wipes all data of the local shard (docker/shard.yml). Continue? [y/N] " answer || true
  [[ $answer =~ ^[Yy]$ ]] || die "aborted (use --yes to skip this question)"
fi

log "building identity-registry binaries"
(cd "$REPO" && cargo build -p identity-registry --bins --quiet)

if ((RESET)); then
  log "resetting the shard"
  # Docker can hold the old host ports for a moment after `down`. A failed
  # `up` leaves broken network endpoints, so each retry starts with `down`.
  for attempt in 1 2 3; do
    "${COMPOSE[@]}" down -v --remove-orphans >/dev/null
    wait_for 60 "the shard host ports to be released" shard_ports_free
    "${COMPOSE[@]}" up -d >/dev/null && all_nodes_running && break
    ((attempt < 3)) || die "could not start every node container (see Troubleshooting in BENCHMARKS.md)"
    log "shard start failed (attempt $attempt), retrying"
  done
fi
log "capping node containers at $MEM_CAP"
docker update --memory "$MEM_CAP" --memory-swap "$MEM_CAP" "${NODES[@]/#/rnode.}" >/dev/null
log "waiting for $CREATOR to make blocks"
wait_for 600 "the first block" shard_making_blocks

if cgroup_dir "$CREATOR" >/dev/null; then
  MEM_METHOD="cgroup v2 memory.stat anon, every ${INTERVAL}s"
  sample_cgroup &
else
  MEM_METHOD="docker stats memory usage, about every 1-2s (no cgroup v2 access)"
  sample_docker_stats &
fi
SAMPLER_PID=$!
log "memory sampling: $MEM_METHOD"

log "deploying the identity registry"
URI=$(deploy_registry) || die "could not deploy the identity registry"
log "registry URI: $URI"

echo "scenario,deploys,blocks,max_deploys_per_block,create_ms,replay_ms_validator1,replay_ms_validator3,peak_anon_mib_bootstrap,peak_anon_mib_validator1,peak_anon_mib_validator2,peak_anon_mib_validator3,peak_anon_mib_readonly,bench_outcomes,status" >"$RESULTS"

# --- Steps ------------------------------------------------------------------

# Runs one burst. Returns 1 if a node restarted, so the sweep must stop.
run_step() {
  local scenario=$1 n=$2 tag=$1-$2
  local since t0 before bench_pid status=ok
  before=$(all_restarts)
  since=$(now); t0=$(now_ms)
  log "step $tag: sending $n deploys"
  # shellcheck disable=SC2046
  "$BIN/registry_bench" --registry-uri "$URI" --state "$STATE" --poll-ms 2000 \
    --count "$n" --drain "$DRAIN" --csv "$OUT/ops-$tag.csv" $(bench_args "$scenario") \
    >"$OUT/bench-$tag.txt" 2>&1 &
  bench_pid=$!
  while kill -0 "$bench_pid" 2>/dev/null; do
    if [[ $(all_restarts) != "$before" ]]; then
      kill "$bench_pid" 2>/dev/null || true
      status=oom
      break
    fi
    sleep 2
  done
  wait "$bench_pid" 2>/dev/null || [[ $status == oom ]] || status=bench_failed

  local blocks
  blocks=$(created_blocks "$since")
  if [[ $status == ok && -n $blocks ]]; then
    local b r end=$((SECONDS + REPLAY_TIMEOUT))
    for b in $(awk '{print $2}' <<<"$blocks"); do
      for r in "${REPLAYERS[@]}"; do
        until [[ -n $(replay_ms "$r" "$b" "$since") ]]; do
          [[ $(all_restarts) == "$before" ]] || { status=oom; break 3; }
          ((SECONDS < end)) || { status=replay_timeout; break 3; }
          sleep 2
        done
      done
    done
  fi
  sleep "$SETTLE"
  [[ $(all_restarts) == "$before" ]] || status=oom

  local t1 nblocks maxk create r1 r3 outcomes
  t1=$(now_ms)
  nblocks=$(grep -c . <<<"$blocks" || true)
  maxk=$(awk 'BEGIN { m = 0 } $3 > m { m = $3 } END { print m }' <<<"$blocks")
  create=$(awk '{ s += $4 } END { print s + 0 }' <<<"$blocks")
  r1=0; r3=0
  local b v
  for b in $(awk '{print $2}' <<<"$blocks"); do
    v=$(replay_ms validator1 "$b" "$since"); r1=$((r1 + ${v:-0}))
    v=$(replay_ms validator3 "$b" "$since"); r3=$((r3 + ${v:-0}))
  done
  declare -A peak=()
  local c p
  while read -r c p; do [[ -n $c ]] && peak[$c]=$p; done < <(peaks "$t0" "$t1")
  outcomes=$(sed -n 's/^outcomes: //p' "$OUT/bench-$tag.txt" | tr ',' ';')
  echo "$scenario,$n,$nblocks,$maxk,$create,$r1,$r3,${peak[bootstrap]:-},${peak[validator1]:-},${peak[validator2]:-},${peak[validator3]:-},${peak[readonly]:-},\"$outcomes\",$status" >>"$RESULTS"
  log "step $tag: $status, blocks=$nblocks create=${create}ms replay=${r1}/${r3}ms peak v2=${peak[validator2]:-?}MiB"
  [[ $status != oom ]]
}

prefill() {
  log "prefilling 16 identities for the resolve scenario (not measured)"
  "$BIN/registry_bench" --registry-uri "$URI" --state "$STATE" --poll-ms 2000 \
    --count 16 --drain "$DRAIN" --scenario register >"$OUT/bench-prefill.txt" 2>&1 \
    || die "prefill failed, see $OUT/bench-prefill.txt"
}

STOPPED=""
for scenario in $SCENARIOS; do
  for n in $SIZES; do
    if [[ $scenario == resolve && ! -s $STATE ]]; then prefill; fi
    if ! run_step "$scenario" "$n"; then
      STOPPED="$scenario-$n"
      log "a node restarted during step $STOPPED; stopping the shard to avoid a crash loop"
      "${COMPOSE[@]}" stop >/dev/null 2>&1
      break 2
    fi
  done
done

# --- Report -----------------------------------------------------------------

{
  echo "# Block cost report"
  echo
  echo "| Field | Value |"
  echo "|---|---|"
  echo "| Date (UTC) | $(date -u +%Y-%m-%dT%H:%M:%SZ) |"
  echo "| Commit | $(git -C "$REPO" rev-parse --short HEAD)$(git -C "$REPO" diff --quiet || echo ' (uncommitted changes)') |"
  echo "| Node image | $(docker inspect -f '{{.Config.Image}}' "rnode.$CREATOR") |"
  echo "| Host | $(uname -sr), $(getconf _NPROCESSORS_ONLN) CPUs |"
  echo "| Memory cap per node | $MEM_CAP |"
  echo "| Memory method | $MEM_METHOD |"
  echo "| Deploys sent to | $CREATOR |"
  [[ -n $STOPPED ]] && echo "| Stopped at | $STOPPED (a node restarted, probably at the memory cap) |"
  echo
  echo "Times in ms. Memory is peak anonymous memory in MiB during the step."
  echo
  echo "| scenario | deploys | blocks | create | replay v1 | replay v3 | peak v2 (creator) | peak v1 | peak v3 | peak bootstrap | peak readonly | outcomes | status |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|"
  tail -n +2 "$RESULTS" | while IFS=, read -r s n nb mk cr r1 r3 pb p1 p2 p3 pr oc st; do
    echo "| $s | $n | $nb | $cr | $r1 | $r3 | $p2 | $p1 | $p3 | $pb | $pr | ${oc//\"/} | $st |"
  done
} >"$OUT/report.md"

log "done: $OUT/report.md"
cat "$OUT/report.md"
