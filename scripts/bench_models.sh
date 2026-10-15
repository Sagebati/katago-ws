#!/usr/bin/env bash
# Benchmark every model in bench/models/ under the same KataGo config, then
# tabulate throughput and quality side by side.
#
#   scripts/bench_models.sh [PREFIX] [cfg.key=value ...] [env.MUXA_X=value ...]
#
#   scripts/bench_models.sh m numAnalysisThreads=256 numSearchThreadsPerAnalysisThread=1
#   MODELS="g170e-b10c128 kata1-b18c384nbt" scripts/bench_models.sh m ...
#
# Each run is `scripts/bench.sh PREFIX-<model> ARGS...` with BENCH_MODEL set, so
# reports land in bench/results/PREFIX-<model>.json. MODELS (space-separated
# basenames without .bin.gz) restricts the set; default is every file in
# bench/models/. Models are run smallest file first, so quick results come early.
#
# Quality is scored against REF (default bench/results/ref-b18-v1000.json, a
# high-visit run of one strong model — make one with BENCH_MAX_VISITS=1000 and
# BENCH_PRESET=quick). A reference made with model X flatters X and its
# relatives, so read the quality columns as relative, not absolute.
# BENCH_PRESET=quick|prod picks the corpus, as in bench.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

prefix=${1:-model}
shift || true
ref=${REF:-bench/results/ref-b18-v1000.json}

if [[ -n ${MODELS:-} ]]; then
  read -ra names <<<"$MODELS"
  files=()
  for name in "${names[@]}"; do files+=("bench/models/$name.bin.gz"); done
else
  mapfile -t files < <(ls -Sr bench/models/*.bin.gz)
fi

reports=()
for file in "${files[@]}"; do
  name=$(basename "$file" .bin.gz)
  label="$prefix-$name"
  echo "== $name"
  BENCH_MODEL="$file" scripts/bench.sh "$label" "$@" |
    grep -oE "bench: done.*|ERROR.*" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-160
  [[ -f bench/results/$label.json ]] && reports+=("bench/results/$label.json")
done

if [[ -f $ref ]]; then
  scripts/bench_compare.py "$ref" "${reports[@]}"
else
  scripts/bench_compare.py "${reports[@]}"
fi
