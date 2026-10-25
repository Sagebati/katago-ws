#!/usr/bin/env bash
# Populate the (gitignored) bench/ workspace for `katago-ws bench`:
#   bench/katago-rs            the katago-rs engine, built from the sibling checkout
#   bench/models/*.bpk         its compiled-in nets: b6c96, b10c128 (the CPU default), b18c384nbt (the GPU
#                              default), b18c384nbt-humanv0 (the human-SL model)
#   bench/cfg/baseline.cfg     KataGo's stock analysis config (what the images ship)
#   bench/sgf-quick/           6 games: BENCH_PRESET=quick, and the quality-reference set
#   bench/sgf/                 small tuning corpus (~17 games)
#   bench/sgf-large/           larger corpus for final numbers (BENCH_GAMES, default 300)
#
#   scripts/bench_setup.sh                       # everything
#   BENCH_GAMES=100 scripts/bench_setup.sh
#   KATAGO_RS=~/src/katago-rs BENCH_FEATURES=flex,wgpu scripts/bench_setup.sh
#
# katago-rs compiles its nets in from ONNX: the .bin.gz nets are downloaded to bench/nets/ and converted with
# katago-rs's tools/bin2onnx (`just convert`, its .venv) when its models/onnx/ lacks them. BENCH_FEATURES picks
# the backends (default flex,wgpu, plus rocm when hipconfig is on the PATH); a rebuild (new katago-rs commit,
# other features) needs `rm bench/katago-rs`.
#
# Games are public 19x19 OGS games of >=120 moves, fetched politely one by one.
# Already-present files are kept, so re-running only fills gaps.
set -euo pipefail
cd "$(dirname "$0")/.."

version=${KATAGO_VERSION:-v1.16.5}
rs=$(realpath "${KATAGO_RS:-../katago-rs}")
games=${BENCH_GAMES:-300}
if [[ -z ${BENCH_FEATURES:-} ]]; then
  BENCH_FEATURES=flex,wgpu
  command -v hipconfig >/dev/null && BENCH_FEATURES+=,rocm
fi

mkdir -p bench/nets bench/models bench/cfg bench/sgf bench/sgf-quick bench/sgf-large bench/results bench/logs

fetch() { [[ -s $1 ]] || curl -fsSL -o "$1" "$2"; }
nets=(
  "g170-b6c96 https://katagoarchive.org/g170/neuralnets/g170-b6c96-s175395328-d26788732.bin.gz"
  "g170e-b10c128 https://katagoarchive.org/g170/neuralnets/g170e-b10c128-s1141046784-d204142634.bin.gz"
  "kata1-b18c384nbt https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b18c384nbt-s9996604416-d4316597426.bin.gz"
  "b18c384nbt-humanv0 https://github.com/lightvector/KataGo/releases/download/v1.15.0/b18c384nbt-humanv0.bin.gz"
)
for net in "${nets[@]}"; do
  read -r name url <<<"$net"
  [[ -s $rs/models/onnx/$name.onnx ]] && continue
  fetch "bench/nets/$name.bin.gz" "$url"
  (cd "$rs" && just convert "$OLDPWD/bench/nets/$name.bin.gz" "$name")
done

if [[ ! -x bench/katago-rs ]]; then
  features="$BENCH_FEATURES,net-b6c96,net-b10c128,net-b18c384nbt,net-b18c384nbt-humanv0"
  (cd "$rs" && cargo build --release --locked -p katago-cli --no-default-features -F "$features" \
    --message-format=json-render-diagnostics) > bench/cargo.json
  # The nets' weights are written by katago-nn-burn's build script, to its OUT_DIR.
  out=$(jq -r 'select(.reason=="build-script-executed" and (.package_id|test("katago-nn-burn"))) | .out_dir' \
    bench/cargo.json | tail -1)
  bin=$(jq -r 'select(.reason=="compiler-artifact" and .executable != null) | .executable' bench/cargo.json | tail -1)
  cp "$out"/model/*.bpk bench/models/
  cp "$bin" bench/katago-rs
  rm bench/cargo.json
fi
bench/katago-rs version

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
    # Already fetched on an earlier run: `have` counted it at the start, so
    # skip it instead of downloading it again and counting it twice.
    [[ -e "$out/ogs-$id.sgf" ]] && continue
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
