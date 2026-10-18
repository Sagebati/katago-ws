#!/usr/bin/env bash
# One benchmark run of the KataGo worker: derive a KataGo config from a base
# file plus KEY=VALUE overrides, then run `katago-ws bench` against it.
#
#   scripts/bench.sh LABEL [cfg.key=value ...] [env.KATAGO_WS_X=value ...]
#
#   scripts/bench.sh threads32x1 numAnalysisThreads=32 numSearchThreadsPerAnalysisThread=1
#   scripts/bench.sh conc4 env.KATAGO_WS_BENCH__CONCURRENCY=4
#
# Bare KEY=VALUE pairs override lines of the KataGo config (uncommenting them if
# needed); `env.` pairs are exported for the run (KATAGO_WS_ENGINE__*, KATAGO_WS_BENCH__*).
# Report → bench/results/LABEL.json, derived config → bench/cfg/LABEL.cfg.
#
# Corpus presets (BENCH_PRESET), all at the same engine settings:
#   quick     bench/sgf-quick, 6 games (~1 min at b20 tuned). Fast A/B checks;
#             also the quality-reference set, so its reports always get the
#             dWR/dSL/best columns. Pipeline ramp-up/drain weighs more on so few
#             games — compare quick runs with quick runs only.
#   (unset)   bench/sgf, 17 games (~3 min). The tuning corpus.
#   prod      bench/sgf-large, ~300 games (~45 min). Sustained, final numbers.
#
# Defaults (override in the environment): BENCH_PRESET, BENCH_BASE_CFG,
# BENCH_MODEL, BENCH_KATAGO, BENCH_CORPUS (wins over the preset),
# BENCH_CONCURRENCY, BENCH_MAX_VISITS. Requires a release build.
#
# The startup auto-tune ([engine] autotune) is OFF here by default, so a run
# measures exactly the config it derives; BENCH_AUTOTUNE=true benchmarks the
# auto-tuned settings instead (keys passed as env.KATAGO_WS_ENGINE__* with a
# non-zero value, or in OVERRIDE_CONFIG, stay pinned).
#
# The derived config file is authoritative: the run zeroes [engine]'s own
# thread/batch knobs (analysis_threads etc.), which would otherwise be passed
# to KataGo as -override-config and win over the file.
set -euo pipefail
cd "$(dirname "$0")/.."

label=${1:?usage: scripts/bench.sh LABEL [key=value ...]}
shift

case ${BENCH_PRESET:-} in
  quick) corpus=bench/sgf-quick ;;
  prod)  corpus=bench/sgf-large ;;
  "")    corpus=bench/sgf ;;
  *)     echo "unknown BENCH_PRESET=$BENCH_PRESET (quick|prod)" >&2; exit 2 ;;
esac
corpus=${BENCH_CORPUS:-$corpus}

base=${BENCH_BASE_CFG:-bench/cfg/baseline.cfg}
cfg=bench/cfg/$label.cfg
mkdir -p bench/cfg bench/results bench/logs
cp "$base" "$cfg"

envs=()
for kv in "$@"; do
  key=${kv%%=*}
  val=${kv#*=}
  if [[ $key == env.* ]]; then
    envs+=("${key#env.}=$val")
  elif grep -qE "^#?[[:space:]]*$key[[:space:]]*=" "$cfg"; then
    # Replace the first (possibly commented) occurrence; drop later ones.
    awk -v key="$key" -v val="$val" '
      $0 ~ "^#?[[:space:]]*" key "[[:space:]]*=" { if (!done) { print key " = " val; done = 1 }; next }
      { print }' "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
  else
    printf '%s = %s\n' "$key" "$val" >> "$cfg"
  fi
done

env RUST_LOG="${RUST_LOG:-info}" \
  KATAGO_WS_ENGINE__BINARY="${BENCH_KATAGO:-bench/katago-dist/katago}" \
  KATAGO_WS_ENGINE__CONFIG="$cfg" \
  KATAGO_WS_ENGINE__MODEL="${BENCH_MODEL:-bench/models/g170e-b10c128.bin.gz}" \
  KATAGO_WS_ENGINE__MAX_VISITS="${BENCH_MAX_VISITS:-100}" \
  KATAGO_WS_ENGINE__ANALYSIS_THREADS=0 \
  KATAGO_WS_ENGINE__SEARCH_THREADS_PER_ANALYSIS=0 \
  KATAGO_WS_ENGINE__NN_MAX_BATCH_SIZE=0 \
  KATAGO_WS_ENGINE__NN_SERVER_THREADS=0 \
  KATAGO_WS_ENGINE__SCREEN_VISITS=0 \
  KATAGO_WS_ENGINE__AUTOTUNE="${BENCH_AUTOTUNE:-false}" \
  KATAGO_WS_BENCH__CORPUS="$corpus" \
  KATAGO_WS_BENCH__CONCURRENCY="${BENCH_CONCURRENCY:-2}" \
  KATAGO_WS_BENCH__LABEL="$label" \
  KATAGO_WS_BENCH__OUT="bench/results/$label.json" \
  "${envs[@]}" \
  ./target/release/katago-ws bench 2>&1 | grep -E "bench:|ERROR|WARN" || true
