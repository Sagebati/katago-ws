# katago-ws image builds. Run from this directory (katago-ws/).
# Build context is the parent dir so the local muxa/pgmq path-deps are included.
#
#   just cpu | just cuda | just opencl | just all
#   just makefile      # emit a standalone Makefile equivalent

image := "katago-ws"
ctx   := ".."

# Per-platform settings — single source of truth for recipes AND `just makefile`.
cpu_base    := "debian:bookworm-slim"
cpu_zip     := "katago-v1.16.5-eigen-linux-x64.zip"
cpu_pkgs    := "ca-certificates"

cuda_base   := "nvidia/cuda:12.8.0-cudnn-runtime-ubuntu24.04"
cuda_zip    := "katago-v1.16.5-cuda12.8-cudnn9.8.0-linux-x64.zip"
cuda_pkgs   := "ca-certificates"

# KataGo has no ROCm backend; AMD (and other) GPUs run via its OpenCL build.
#
# trixie, not bookworm: bookworm's Mesa 22.3 / libclc 15 ships amdgcn bitcode
# only up to gfx906, so every kernel build on an RDNA2+ card dies with
# "cannot open file '/usr/lib/clc/gfx1030-amdgcn-mesa-mesa3d.bc'". trixie's
# Mesa 25.0 / libclc 19 covers gfx10xx/gfx11xx and has a working rusticl.
opencl_base    := "debian:trixie-slim"
opencl_zip     := "katago-v1.16.5-opencl-linux-x64.zip"
opencl_pkgs    := "ca-certificates ocl-icd-libopencl1 mesa-opencl-icd clinfo"
# Which gallium drivers rusticl exposes. AMD + Intel; NVIDIA users want :cuda.
opencl_rusticl := "radeonsi,iris"

# List recipes.
default:
    @just --list

# Run the test suite with cargo-nextest (https://nexte.st).
# Install once with: cargo install cargo-nextest --locked
# Pass filters/flags through, e.g. `just test sgf` or `just test --no-capture`.
test *ARGS:
    cargo nextest run {{ARGS}}

# CPU (Eigen) — pure Debian, no GPU.
cpu:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:cpu \
      --build-arg RUNTIME_BASE={{cpu_base}} \
      --build-arg KATAGO_ZIP={{cpu_zip}} \
      --build-arg RUNTIME_PKGS="{{cpu_pkgs}}" \
      {{ctx}}

# NVIDIA CUDA. Base + KataGo build must agree on CUDA/cuDNN versions.
# Deploy needs nvidia-container-toolkit + `docker run --gpus all ...`.
cuda:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:cuda \
      --build-arg RUNTIME_BASE={{cuda_base}} \
      --build-arg KATAGO_ZIP={{cuda_zip}} \
      --build-arg RUNTIME_PKGS="{{cuda_pkgs}}" \
      {{ctx}}

# OpenCL — AMD/Intel GPUs. Debian + Mesa rusticl ICD.
# Deploy with `--device /dev/dri`; the image selects rusticl on its own.
opencl:
    DOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:opencl \
      --build-arg RUNTIME_BASE={{opencl_base}} \
      --build-arg KATAGO_ZIP={{opencl_zip}} \
      --build-arg RUNTIME_PKGS="{{opencl_pkgs}}" \
      --build-arg RUSTICL_DRIVERS="{{opencl_rusticl}}" \
      {{ctx}}

# Build all three.
all: cpu cuda opencl

# Compile this justfile into a standalone Makefile (runs without `just`).
makefile:
    #!/usr/bin/env bash
    set -euo pipefail
    # $5 is the rusticl driver list; only the opencl target passes one.
    target() {
      printf '%s:\n\tDOCKER_BUILDKIT=1 docker build -f Dockerfile -t {{image}}:%s --build-arg RUNTIME_BASE=%s --build-arg KATAGO_ZIP=%s --build-arg RUNTIME_PKGS="%s"%s {{ctx}}\n\n' \
        "$1" "$1" "$2" "$3" "$4" "${5:+ --build-arg RUSTICL_DRIVERS=\"$5\"}"
    }
    {
      printf '# Generated from the justfile by `just makefile`. Do not edit by hand.\n'
      printf '.PHONY: cpu cuda opencl all\n\n'
      target cpu    '{{cpu_base}}'    '{{cpu_zip}}'    '{{cpu_pkgs}}'
      target cuda   '{{cuda_base}}'   '{{cuda_zip}}'   '{{cuda_pkgs}}'
      target opencl '{{opencl_base}}' '{{opencl_zip}}' '{{opencl_pkgs}}' '{{opencl_rusticl}}'
      printf 'all: cpu cuda opencl\n'
    } > Makefile
    echo "Wrote Makefile"
