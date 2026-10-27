# katago-ws. Run from this directory (katago-ws/).
#
#   just check                          # what CI runs: fmt, clippy, nextest
#   just autotune | just bench LABEL    # the engine on this host
#   just cpu | just vulkan | just rocm  # images
#   just makefile                       # emit a standalone Makefile for the images

image := "katago-ws"
# The image build context is the parent dir so the local katago-rs/muxa/pgmq
# siblings are included.
ctx   := ".."

# The engine is katago-rs (the sibling repo), built in the image with one compute
# backend and the networks it compiles in. Per-platform settings — single source
# of truth for recipes, CI's matrix (`just print-matrix`) and `just makefile`.
nets_gpu := "net-b10c128,net-b18c384nbt,net-b18c384nbt-humanv0"

# CPU — burn flex, b10c128 (b18 runs ~1 eval/s on a CPU). Pure Debian, no GPU.
cpu_base     := "debian:bookworm-slim"
cpu_pkgs     := "ca-certificates"
cpu_features := "flex,net-b10c128"
cpu_backend  := "flex"
cpu_fp16     := "false"
cpu_model    := "g170e-b10c128.bpk"

# Vulkan — wgpu, for any GPU with a Vulkan driver: Mesa RADV (AMD), ANV (Intel),
# or NVIDIA's (the NVIDIA container toolkit injects it: `--gpus all`; the image
# asks for the `graphics` capability). AMD/Intel: `--device /dev/dri`.
# trixie for a recent Mesa (RADV on RDNA2+).
vulkan_base     := "debian:trixie-slim"
vulkan_pkgs     := "ca-certificates libvulkan1 mesa-vulkan-drivers"
vulkan_features := "wgpu," + nets_gpu
vulkan_backend  := "wgpu"
vulkan_fp16     := "true"
vulkan_model    := "kata1-b18c384nbt.bpk"

# ROCm — HIP on AMD GPUs ROCm supports (RDNA2+/CDNA, e.g. RX 6800 XT): the fastest
# path measured. RX 6800 XT, kata1 b18c384nbt, 100 visits, f16, 8 server threads
# x batch 64, 256 analysis threads: 13.4 positions/s (KataGo v1.16.5 OpenCL: 7.1).
# Only the HIP runtime from AMD's repo (hiprtc JIT-compiles katago-rs's kernels
# for whichever GPU is present). Ubuntu, not Debian, on purpose: AMD's apt repo
# targets Ubuntu releases. The container's ROCm needn't match the host's — only
# the host kernel's amdgpu/KFD driver is used. Deploy with the KFD + DRM devices
# and the host's video/render GIDs (no --privileged):
#   docker run --device /dev/kfd --device /dev/dri \
#     --group-add "$(getent group video | cut -d: -f3)" \
#     --group-add "$(getent group render | cut -d: -f3)" katago-ws:rocm worker
rocm_version  := "7.2.3"
rocm_base     := "ubuntu:24.04"
rocm_key_url  := "https://repo.radeon.com/rocm/rocm.gpg.key"
rocm_repo     := "https://repo.radeon.com/rocm/apt/" + rocm_version + " noble main"
rocm_pkgs     := "ca-certificates libstdc++6 hip-runtime-amd rocm-device-libs"
rocm_features := "rocm," + nets_gpu
rocm_backend  := "rocm"
rocm_fp16     := "true"
rocm_model    := "kata1-b18c384nbt.bpk"

# List recipes.
default:
    @just --list --unsorted

# ---------------------------------------------------------------------------
# Checks — the same three steps as .github/workflows/ci.yml.
# ---------------------------------------------------------------------------

# Format the sources.
fmt:
    cargo fmt

# Clippy with warnings as errors, as CI runs it.
lint:
    cargo clippy --all-targets -- -D warnings

# Pass filters/flags through, e.g. `just test sgf` or `just test --no-capture`.
# Install cargo-nextest (https://nexte.st) once with:
#   cargo install cargo-nextest --locked

# Run the test suite with cargo-nextest.
test *ARGS:
    cargo nextest run {{ARGS}}

# The CI gate: formatting, clippy, tests.
check:
    cargo fmt --check
    @just lint
    @just test
    @echo "✔ katago-ws checks passed"

# ---------------------------------------------------------------------------
# The engine on this host: auto-tune and benchmarks
# ---------------------------------------------------------------------------

# `just autotune --fast` = the few-trial search; --force re-tunes over a cached
# result. Uses [engine] from config.toml / the env; the engine roles start from
# the cache it writes.

# Auto-tune the engine for this host into the cache, then exit.
autotune *ARGS:
    cargo build --release
    ./target/release/katago-ws autotune {{ARGS}}

# One-time setup: scripts/bench_setup.sh (katago-rs build + nets, SGF corpus → bench/).
# Args are engine config overrides or env.KATAGO_WS_*=… — see scripts/bench.sh, e.g.
#   just bench t256 numAnalysisThreads=256 numSearchThreadsPerAnalysisThread=1
# Compare runs: scripts/bench_compare.py bench/results/BASE.json bench/results/*.json

# Benchmark the engine: one labelled run of the `bench` role (17 games).
bench LABEL *ARGS:
    cargo build --release
    scripts/bench.sh {{LABEL}} {{ARGS}}

# Quick bench: 6 games, ~1 min — for fast A/B checks (compare quick with quick).
bench-quick LABEL *ARGS:
    cargo build --release
    BENCH_PRESET=quick scripts/bench.sh {{LABEL}} {{ARGS}}

# Production bench: the ~300-game corpus, ~45 min — sustained, final numbers.
bench-prod LABEL *ARGS:
    cargo build --release
    BENCH_PRESET=prod scripts/bench.sh {{LABEL}} {{ARGS}}

# Bench every model in bench/models/ with the same args (BENCH_PRESET applies).
bench-models PREFIX *ARGS:
    cargo build --release
    scripts/bench_models.sh {{PREFIX}} {{ARGS}}

# ---------------------------------------------------------------------------
# Images
# ---------------------------------------------------------------------------

# One image; `just cpu`, `just vulkan` and `just rocm` call it.
[private]
build variant base pkgs features backend fp16 model key_url="" repo="":
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:{{variant}} \
      --build-arg RUNTIME_BASE={{base}} \
      --build-arg RUNTIME_PKGS="{{pkgs}}" \
      --build-arg RUNTIME_APT_GPG_URL="{{key_url}}" \
      --build-arg RUNTIME_APT_REPO="{{repo}}" \
      --build-arg ENGINE_FEATURES="{{features}}" \
      --build-arg ENGINE_BACKEND={{backend}} \
      --build-arg ENGINE_FP16={{fp16}} \
      --build-arg ENGINE_MODEL={{model}} \
      {{ctx}}

# CPU — katago-rs on burn flex, b10c128.
cpu: (build "cpu" cpu_base cpu_pkgs cpu_features cpu_backend cpu_fp16 cpu_model)

# Vulkan — katago-rs on wgpu, any GPU with a Vulkan driver, b18c384nbt.
vulkan: (build "vulkan" vulkan_base vulkan_pkgs vulkan_features vulkan_backend vulkan_fp16 vulkan_model)

# ROCm — katago-rs on HIP, AMD GPUs, b18c384nbt.
rocm: (build "rocm" rocm_base rocm_pkgs rocm_features rocm_backend rocm_fp16 rocm_model rocm_key_url rocm_repo)

# Build all three.
all: cpu vulkan rocm

# Consumed by .github/workflows/{image,release}.yml via fromJson() so the
# variants never need a second, independently-maintained copy in YAML.

# Emit the CI build matrix as JSON: {"include":[{variant,base,pkgs,features,backend,fp16,model,key_url,repo}...]}.
print-matrix:
    @printf '{"include":[%s,%s,%s]}\n' \
      '{"variant":"cpu","base":"{{cpu_base}}","pkgs":"{{cpu_pkgs}}","features":"{{cpu_features}}","backend":"{{cpu_backend}}","fp16":"{{cpu_fp16}}","model":"{{cpu_model}}","key_url":"","repo":""}' \
      '{"variant":"vulkan","base":"{{vulkan_base}}","pkgs":"{{vulkan_pkgs}}","features":"{{vulkan_features}}","backend":"{{vulkan_backend}}","fp16":"{{vulkan_fp16}}","model":"{{vulkan_model}}","key_url":"","repo":""}' \
      '{"variant":"rocm","base":"{{rocm_base}}","pkgs":"{{rocm_pkgs}}","features":"{{rocm_features}}","backend":"{{rocm_backend}}","fp16":"{{rocm_fp16}}","model":"{{rocm_model}}","key_url":"{{rocm_key_url}}","repo":"{{rocm_repo}}"}'

# Compile the image recipes into a standalone Makefile (runs without `just`).
makefile:
    #!/usr/bin/env bash
    set -euo pipefail
    target() { # variant base pkgs features backend fp16 model key_url repo
      printf '%s:\n\tDOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:%s --build-arg RUNTIME_BASE=%s --build-arg RUNTIME_PKGS="%s" --build-arg RUNTIME_APT_GPG_URL="%s" --build-arg RUNTIME_APT_REPO="%s" --build-arg ENGINE_FEATURES="%s" --build-arg ENGINE_BACKEND=%s --build-arg ENGINE_FP16=%s --build-arg ENGINE_MODEL=%s {{ctx}}\n\n' \
        "$1" "$1" "$2" "$3" "$8" "$9" "$4" "$5" "$6" "$7"
    }
    {
      printf '# Generated from the justfile by `just makefile`. Do not edit by hand.\n'
      printf '.PHONY: cpu vulkan rocm all\n\n'
      target cpu    '{{cpu_base}}'    '{{cpu_pkgs}}'    '{{cpu_features}}'    '{{cpu_backend}}'    '{{cpu_fp16}}'    '{{cpu_model}}'    '' ''
      target vulkan '{{vulkan_base}}' '{{vulkan_pkgs}}' '{{vulkan_features}}' '{{vulkan_backend}}' '{{vulkan_fp16}}' '{{vulkan_model}}' '' ''
      target rocm   '{{rocm_base}}'   '{{rocm_pkgs}}'   '{{rocm_features}}'   '{{rocm_backend}}'   '{{rocm_fp16}}'   '{{rocm_model}}'   '{{rocm_key_url}}' '{{rocm_repo}}'
      printf 'all: cpu vulkan rocm\n'
    } > Makefile
    echo "Wrote Makefile"
