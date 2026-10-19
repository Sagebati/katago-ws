#!/usr/bin/env bash
# Populate the (gitignored) bench/ workspace for `katago-ws bench`:
#   bench/katago-dist/katago   KataGo release binary (OpenCL build by default)
#   bench/models/*.bin.gz      the default b10 + a size range of g170/kata1 nets
#   bench/cfg/baseline.cfg     KataGo's stock analysis config (what the image ships)
#   bench/sgf-quick/           6 games: BENCH_PRESET=quick, and the quality-reference set
#   bench/sgf/                 small tuning corpus (~17 games)
#   bench/sgf-large/           larger corpus for final numbers (BENCH_GAMES, default 300)
#
#   scripts/bench_setup.sh                 # everything
#   BENCH_GAMES=100 scripts/bench_setup.sh
#   KATAGO_ZIP=katago-v1.16.5-cuda12.8-cudnn9.8.0-linux-x64.zip scripts/bench_setup.sh
#
# Games are public 19x19 OGS games of >=120 moves, fetched politely one by one.
# Already-present files are kept, so re-running only fills gaps.
set -euo pipefail
cd "$(dirname "$0")/.."

version=${KATAGO_VERSION:-v1.16.5}
zip=${KATAGO_ZIP:-katago-$version-opencl-linux-x64.zip}
games=${BENCH_GAMES:-300}

mkdir -p bench/models bench/cfg bench/sgf bench/sgf-quick bench/sgf-large bench/results bench/logs

if [[ ! -x bench/katago-dist/katago ]]; then
  curl -fsSL -o bench/katago.zip "https://github.com/lightvector/KataGo/releases/download/$version/$zip"
  unzip -o -q bench/katago.zip -d bench/katago-dist
fi

fetch() { [[ -s $1 ]] || curl -fsSL -o "$1" "$2"; }
fetch bench/models/g170e-b20c256x2.bin.gz \
  https://github.com/lightvector/KataGo/releases/download/v1.4.5/g170e-b20c256x2-s5303129600-d1228401921.bin.gz
fetch bench/models/kata1-b18c384nbt.bin.gz \
  https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b18c384nbt-s9996604416-d4316597426.bin.gz
# The rest of the size range, for `just bench-models` (small → large).
fetch bench/models/g170-b6c96.bin.gz \
  https://katagoarchive.org/g170/neuralnets/g170-b6c96-s175395328-d26788732.bin.gz
fetch bench/models/g170e-b10c128.bin.gz \
  https://katagoarchive.org/g170/neuralnets/g170e-b10c128-s1141046784-d204142634.bin.gz
fetch bench/models/g170e-b15c192.bin.gz \
  https://katagoarchive.org/g170/neuralnets/g170e-b15c192-s1672170752-d466197061.bin.gz
fetch bench/models/kata1-b40c256.bin.gz \
  https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b40c256-s12860905472-d3197353276.bin.gz
fetch bench/models/kata1-b28c512nbt.bin.gz \
  https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b28c512nbt-s13255194368-d5935380940.bin.gz
fetch bench/analysis_example.cfg \
  "https://raw.githubusercontent.com/lightvector/KataGo/$version/cpp/configs/analysis_example.cfg"
sed 's#^logDir.*#logDir = bench/logs#' bench/analysis_example.cfg > bench/cfg/baseline.cfg

# Small tuning corpus first, then the large one (which includes the small one).
fetch_games() {
  local out=$1 want=$2 start=$3 have
  have=$(find "$out" -name '*.sgf' | wc -l)
  for id in $(seq "$start" 1931 70000000); do
    (( have >= want )) && break
    local sgf moves
    sgf=$(curl -fsSL --max-time 10 "https://online-go.com/api/v1/games/$id/sgf" 2>/dev/null) || continue
    grep -q 'SZ\[19\]' <<<"$sgf" || continue
    # `|| true`: a 19x19 game with no moves makes grep exit 1, which pipefail +
    # set -e would turn into a silent exit of the whole script.
    moves=$({ grep -oE ';[BW]\[[a-s]{2}\]' <<<"$sgf" || true; } | wc -l)
    (( moves >= 120 )) || continue
    printf '%s\n' "$sgf" > "$out/ogs-$id.sgf"
    have=$((have + 1))
    sleep 0.25
  done
}
fetch_games bench/sgf 17 60000000
cp -n bench/sgf/*.sgf bench/sgf-large/
find bench/sgf -name '*.sgf' | sort | head -6 | xargs -I{} cp -n {} bench/sgf-quick/
fetch_games bench/sgf-large "$games" 61000000

echo "bench/ ready: $(find bench/sgf-quick -name '*.sgf' | wc -l) quick, $(find bench/sgf -name '*.sgf' | wc -l) tuning, $(find bench/sgf-large -name '*.sgf' | wc -l) large-corpus games"
