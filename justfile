# katago-ws image builds. Run from this directory (katago-ws/).
# Build context is the parent dir so the local muxa/pgmq path-deps are included.
#
#   just cpu | just cuda | just opencl | just rocm | just rocm-hip | just all
#   just makefile      # emit a standalone Makefile equivalent

image := "katago-ws"
ctx   := ".."

# KataGo engine version — single source of truth (interpolated into the three
# zip names below and passed explicitly as a build-arg, rather than relying on
# the Dockerfile's own default). CI derives its build matrix from this file via
# `just print-matrix`, so this is the only place the version needs to change.
katago_version := "v1.16.5"

# Per-platform settings — single source of truth for recipes AND `just makefile`.
cpu_base    := "debian:bookworm-slim"
cpu_zip     := "katago-" + katago_version + "-eigen-linux-x64.zip"
cpu_pkgs    := "ca-certificates"

# NVIDIA's official CUDA+cuDNN image, which only comes in Ubuntu (no Debian
# flavour), so this one stays Ubuntu; every image we assemble ourselves is Debian.
cuda_base   := "nvidia/cuda:12.8.0-cudnn-runtime-ubuntu24.04"
cuda_zip    := "katago-" + katago_version + "-cuda12.8-cudnn9.8.0-linux-x64.zip"
cuda_pkgs   := "ca-certificates"

# KataGo v1.16.5 has no ROCm backend; AMD (and other) GPUs run via its OpenCL
# build (`opencl`, `rocm`), or KataGo master's native one (`rocm-hip`).
#
# trixie, not bookworm: bookworm's Mesa 22.3 / libclc 15 ships amdgcn bitcode
# only up to gfx906, so every kernel build on an RDNA2+ card dies with
# "cannot open file '/usr/lib/clc/gfx1030-amdgcn-mesa-mesa3d.bc'". trixie's
# Mesa 25.0 / libclc 19 covers gfx10xx/gfx11xx and has a working rusticl.
opencl_base    := "debian:trixie-slim"
opencl_zip     := "katago-" + katago_version + "-opencl-linux-x64.zip"
opencl_pkgs    := "ca-certificates ocl-icd-libopencl1 mesa-opencl-icd clinfo"
# Which gallium drivers rusticl exposes. AMD + Intel; NVIDIA users want :cuda.
opencl_rusticl := "radeonsi,iris"

# AMD's ROCm userspace from repo.radeon.com. Ubuntu, not Debian, on purpose:
# AMD's official ROCm images are Ubuntu-only (rocm/dev-ubuntu-*; no Debian) and
# its apt repo targets Ubuntu releases, so these images stay on the distro AMD
# builds and supports ROCm for — Ubuntu 24.04, `noble` packages. The container's
# ROCm needn't match the host's — only the host kernel's amdgpu/KFD driver is
# used. Deploy both images with the KFD + DRM devices and the host's video/render
# GIDs (no --privileged):
#   docker run --device /dev/kfd --device /dev/dri \
#     --group-add "$(getent group video | cut -d: -f3)" \
#     --group-add "$(getent group render | cut -d: -f3)" katago-ws:rocm worker
rocm_version  := "7.2.3"
rocm_base     := "ubuntu:24.04"
rocm_key_url  := "https://repo.radeon.com/rocm/rocm.gpg.key"
rocm_repo     := "https://repo.radeon.com/rocm/apt/" + rocm_version + " noble main"

# `rocm`: OpenCL KataGo v1.16.5 built from source with patches/ (a command
# queue per NN server thread, ~2.2x positions/s) on AMD's ROCm OpenCL ICD — only
# the OpenCL runtime (rocm-opencl → comgr + hsa-rocr). The small AMD default.
rocm_backend  := "OPENCL"
rocm_pkgs     := "ca-certificates ocl-icd-libopencl1 libzip4t64 rocm-opencl clinfo"

# `rocm-hip`: KataGo master (v1.18.2+, pinned commit) with `USE_BACKEND=ROCM`
# (MIOpen + hipBLAS). The runtime keeps only the math libraries' kernels for
# these GPU archs: RDNA2 (gfx1030: RX 6800/6900), RDNA3 (gfx1100/1101: RX
# 7900/7800/7700) and RDNA4 (gfx1200/1201: RX 9060/9070; ~585 MB of the image).
# Datacenter archs (gfx90a MI200, gfx942 MI300) work the same way but hipBLASLt
# ships GBs of kernels for them; add them only if you deploy there.
rocm_hip_backend    := "ROCM"
rocm_hip_ref        := "d91ea855110dae533f0aada947b2b7d78cc8a4e1"
rocm_hip_archs      := "gfx1030 gfx1100 gfx1101 gfx1200 gfx1201"
rocm_hip_build_pkgs := "ca-certificates git cmake ninja-build g++ zlib1g-dev libzip-dev rocm-dev miopen-hip-dev hipblas-dev rocblas-dev"
rocm_hip_pkgs       := "ca-certificates libzip4t64 libatomic1 hip-runtime-amd miopen-hip hipblas rocblas rocsolver hipblaslt"

# List recipes.
default:
    @just --list

# Run the test suite with cargo-nextest (https://nexte.st).
# Install once with: cargo install cargo-nextest --locked
# Pass filters/flags through, e.g. `just test sgf` or `just test --no-capture`.
test *ARGS:
    cargo nextest run {{ARGS}}

# Benchmark the KataGo worker: one labelled run of the `bench` role (17 games).
# One-time setup: scripts/bench_setup.sh (KataGo, models, SGF corpus → bench/).
# Args are KataGo config overrides or env.KATAGO_WS_*=… — see scripts/bench.sh, e.g.
#   just bench t256 numAnalysisThreads=256 numSearchThreadsPerAnalysisThread=1
# Compare runs: scripts/bench_compare.py bench/results/BASE.json bench/results/*.json
bench LABEL *ARGS:
    cargo build --release
    scripts/bench.sh {{LABEL}} {{ARGS}}

# Auto-tune KataGo for this host into the cache, then exit (the engine roles
# start from it). `just autotune --fast` = the under-30 s search; --force
# re-tunes over a cached result. Uses [engine] from config.toml / the env.
autotune *ARGS:
    cargo build --release
    ./target/release/katago-ws autotune {{ARGS}}

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

# CPU (Eigen) — pure Debian, no GPU.
cpu:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:cpu \
      --build-arg RUNTIME_BASE={{cpu_base}} \
      --build-arg KATAGO_VERSION={{katago_version}} \
      --build-arg KATAGO_ZIP={{cpu_zip}} \
      --build-arg RUNTIME_PKGS="{{cpu_pkgs}}" \
      {{ctx}}

# NVIDIA CUDA. Base + KataGo build must agree on CUDA/cuDNN versions.
# Deploy needs nvidia-container-toolkit + `docker run --gpus all ...`.
cuda:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:cuda \
      --build-arg RUNTIME_BASE={{cuda_base}} \
      --build-arg KATAGO_VERSION={{katago_version}} \
      --build-arg KATAGO_ZIP={{cuda_zip}} \
      --build-arg RUNTIME_PKGS="{{cuda_pkgs}}" \
      {{ctx}}

# OpenCL — AMD/Intel GPUs. Debian + Mesa rusticl ICD.
# Deploy with `--device /dev/dri`; the image selects rusticl on its own.
opencl:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:opencl \
      --build-arg RUNTIME_BASE={{opencl_base}} \
      --build-arg KATAGO_VERSION={{katago_version}} \
      --build-arg KATAGO_ZIP={{opencl_zip}} \
      --build-arg RUNTIME_PKGS="{{opencl_pkgs}}" \
      --build-arg RUSTICL_DRIVERS="{{opencl_rusticl}}" \
      {{ctx}}

# ROCm — patched OpenCL KataGo on AMD's ROCm OpenCL ICD. Pick this over
# `opencl` on AMD GPUs that ROCm supports (RDNA2+/CDNA, e.g. RX 6800 XT): rusticl
# is ~1.6x slower. RX 6800 XT, 100 visits, one engine, 8 server threads x batch
# 32: b10 ~168 positions/s (stock KataGo: ~75), b20 ~35 (stock: ~26):
#   KATAGO_WS_ENGINE__OVERRIDE_CONFIG="numAnalysisThreads=256,numSearchThreadsPerAnalysisThread=1,nnMaxBatchSize=32,numNNServerThreadsPerModel=8"
# Run one worker per GPU. KataGo tunes its OpenCL kernels on first start (~90 s),
# cached in /home/app/.katago/opencltuning — mount a volume there.
rocm:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:rocm \
      --build-arg RUNTIME_BASE={{rocm_base}} \
      --build-arg KATAGO_BUILD_BASE={{rocm_base}} \
      --build-arg KATAGO_BUILD_BACKEND={{rocm_backend}} \
      --build-arg RUNTIME_PKGS="{{rocm_pkgs}}" \
      --build-arg RUNTIME_APT_KEY_URL={{rocm_key_url}} \
      --build-arg RUNTIME_APT_REPO="{{rocm_repo}}" \
      {{ctx}}

# Native ROCm (HIP) — KataGo master's MIOpen/hipBLAS backend instead of OpenCL.
# Only ~7% faster than `rocm` with the small default net (b10 ~180 vs ~168
# positions/s, RX 6800 XT, 100 visits) and even with b20 (~35), at a ~4 GB image
# (MIOpen, rocSOLVER, rocRAND, rocBLAS are fat binaries); `rocm` stays the
# default. Its best setting is 2 server threads x batch 128:
#   KATAGO_WS_ENGINE__OVERRIDE_CONFIG="numAnalysisThreads=256,numSearchThreadsPerAnalysisThread=1,nnMaxBatchSize=128,numNNServerThreadsPerModel=2"
# Keep the FP16/NCHW defaults (rocmUseNHWC: -45%, rocmUseFP16=false: -34%). No
# OpenCL tuning pass: startup is ~7-12 s (MIOpen compiles per batch size, cached
# in /home/app/.cache/miopen). The openclDeviceToUse* keys don't apply.
rocm-hip:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:rocm-hip \
      --build-arg RUNTIME_BASE={{rocm_base}} \
      --build-arg KATAGO_BUILD_BASE={{rocm_base}} \
      --build-arg KATAGO_BUILD_BACKEND={{rocm_hip_backend}} \
      --build-arg KATAGO_SRC_REF={{rocm_hip_ref}} \
      --build-arg KATAGO_BUILD_PKGS="{{rocm_hip_build_pkgs}}" \
      --build-arg HIP_ARCHS="{{rocm_hip_archs}}" \
      --build-arg RUNTIME_PKGS="{{rocm_hip_pkgs}}" \
      --build-arg RUNTIME_APT_KEY_URL={{rocm_key_url}} \
      --build-arg RUNTIME_APT_REPO="{{rocm_repo}}" \
      {{ctx}}

# Build all five.
all: cpu cuda opencl rocm rocm-hip

# Emit the CI build matrix as JSON: {"include":[{variant,base,zip,pkgs,rusticl,katago_version}...]}.
# Consumed by .github/workflows/{image,release}.yml via fromJson() so the
# variant trios never need a second, independently-maintained copy in YAML.
# `rusticl` is "none" for cpu/cuda (matches the Dockerfile's own ARG default).
print-matrix:
    @printf '{"katago_version":"%s","include":[%s,%s,%s]}\n' \
      '{{katago_version}}' \
      '{"variant":"cpu","base":"{{cpu_base}}","zip":"{{cpu_zip}}","pkgs":"{{cpu_pkgs}}","rusticl":"none"}' \
      '{"variant":"cuda","base":"{{cuda_base}}","zip":"{{cuda_zip}}","pkgs":"{{cuda_pkgs}}","rusticl":"none"}' \
      '{"variant":"opencl","base":"{{opencl_base}}","zip":"{{opencl_zip}}","pkgs":"{{opencl_pkgs}}","rusticl":"{{opencl_rusticl}}"}'

# Compile this justfile into a standalone Makefile (runs without `just`).
makefile:
    #!/usr/bin/env bash
    set -euo pipefail
    # $3 is the KataGo build-arg(s): KATAGO_ZIP=… (release) or KATAGO_BUILD_BACKEND=… (source).
    # $5 is any extra build-args (rusticl drivers, the ROCm apt repo).
    target() {
      printf '%s:\n\tDOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:%s --build-arg RUNTIME_BASE=%s --build-arg %s --build-arg RUNTIME_PKGS="%s"%s {{ctx}}\n\n' \
        "$1" "$1" "$2" "$3" "$4" "${5:-}"
    }
    rocm_repo_args=' --build-arg KATAGO_BUILD_BASE={{rocm_base}} --build-arg RUNTIME_APT_KEY_URL={{rocm_key_url}} --build-arg RUNTIME_APT_REPO="{{rocm_repo}}"'
    {
      printf '# Generated from the justfile by `just makefile`. Do not edit by hand.\n'
      printf '.PHONY: cpu cuda opencl rocm rocm-hip all\n\n'
      target cpu    '{{cpu_base}}'    'KATAGO_ZIP={{cpu_zip}}'    '{{cpu_pkgs}}'
      target cuda   '{{cuda_base}}'   'KATAGO_ZIP={{cuda_zip}}'   '{{cuda_pkgs}}'
      target opencl '{{opencl_base}}' 'KATAGO_ZIP={{opencl_zip}}' '{{opencl_pkgs}}' ' --build-arg RUSTICL_DRIVERS="{{opencl_rusticl}}"'
      target rocm   '{{rocm_base}}'   'KATAGO_BUILD_BACKEND={{rocm_backend}}' '{{rocm_pkgs}}' "$rocm_repo_args"
      target rocm-hip '{{rocm_base}}' 'KATAGO_BUILD_BACKEND={{rocm_hip_backend}} --build-arg KATAGO_SRC_REF={{rocm_hip_ref}} --build-arg KATAGO_BUILD_PKGS="{{rocm_hip_build_pkgs}}" --build-arg HIP_ARCHS="{{rocm_hip_archs}}"' '{{rocm_hip_pkgs}}' "$rocm_repo_args"
      printf 'all: cpu cuda opencl rocm rocm-hip\n'
    } > Makefile
    echo "Wrote Makefile"
