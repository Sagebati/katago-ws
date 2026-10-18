# katago-ws — per-platform deployable image.
#
# One Dockerfile, parameterized by build-args, builds a CPU / CUDA / OpenCL
# (Mesa) / ROCm (AMD OpenCL) / ROCm-HIP image. See the justfile for the
# ready-made targets, or:
#
#   docker build -f katago-ws/Dockerfile -t katago-ws:cpu .     # from the PARENT dir
#
# The Rust binary is a standard glibc (Debian) build. KataGo comes either from a
# release zip (KATAGO_ZIP: an AppImage) or, with KATAGO_BUILD_BACKEND set, is
# built from source (the rocm/rocm-hip images; patches/README.md). Its GPU
# builds dynamically link the vendor runtime supplied by the base image.
#
# Build context must be the PARENT directory (contains katago-ws, muxa, pgmq).

# syntax=docker/dockerfile:1

ARG RUNTIME_BASE=debian:bookworm-slim
ARG KATAGO_VERSION=v1.16.5
ARG KATAGO_ZIP=katago-v1.16.5-eigen-linux-x64.zip
# Default net: g170e b10c128, chosen for throughput. Measured with `just
# bench-models` on an RX 6800 XT at 100 visits, positions/s with one engine:
# b10c128 72-85, g170e b20c256x2 26, kata1 b40c256 14, kata1 b18c384nbt 6.4.
# The cost is accuracy: mean win-rate deviation from a strong reference
# (b18c384nbt at 1000 visits) is 6.8% for b10 vs 3.2% for b20 and ~1% for b18.
# For stronger analysis pass a different MODEL_URL, e.g. kata1 b18c384nbt:
#   https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b18c384nbt-s9996604416-d4316597426.bin.gz
ARG MODEL_URL=https://katagoarchive.org/g170/neuralnets/g170e-b10c128-s1141046784-d204142634.bin.gz
ARG CONFIG_URL=https://raw.githubusercontent.com/lightvector/KataGo/${KATAGO_VERSION}/cpp/configs/analysis_example.cfg
ARG RUNTIME_PKGS=ca-certificates
# Empty: use the release binary from KATAGO_ZIP. A KataGo backend (OPENCL,
# ROCM): build KATAGO_SRC_REF (a tag or full commit) from source with any
# patches/katago-${KATAGO_SRC_REF}-*.patch applied, and skip the zip. A source
# build is a plain ELF, so RUNTIME_PKGS must then provide its shared libs.
ARG KATAGO_BUILD_BACKEND=
ARG KATAGO_SRC_REF=v1.16.5
# Base image and apt packages of the source-build stage (the defaults build the
# OpenCL backend). The build base must match RUNTIME_BASE's distro release.
ARG KATAGO_BUILD_BASE=debian:trixie-slim
ARG KATAGO_BUILD_PKGS="ca-certificates git cmake ninja-build g++ zlib1g-dev libzip-dev opencl-headers ocl-icd-opencl-dev"
# ROCM backend only: the AMD GPU archs to compile for (space-separated). The
# runtime stage also deletes rocBLAS/hipBLASLt/MIOpen per-arch kernel files for
# every other arch, which is most of their size.
ARG HIP_ARCHS=
# Optional extra apt repo for the source-build and runtime stages (AMD's ROCm
# repo): an ASCII-armored signing key URL and the rest of a `deb` line.
ARG RUNTIME_APT_KEY_URL=
ARG RUNTIME_APT_REPO=

###############################################################################
# Stage 1 — build the Rust binary (glibc / Debian)
#
# Used by the local/`just` path and by the main-push image build, both of
# which compile in-container. The tag-triggered release build compiles once
# on the runner instead and skips straight to `runtime-prebuilt` below, so
# the (slow, LTO) compile isn't repeated per variant.
###############################################################################
FROM rust:1.95.0-bookworm AS build-binary

# pkg-config + libpq-dev let pq-sys (pulled transitively by diesel) link; the
# binary doesn't actually call libpq (diesel-async is pure Rust), so it's
# dropped at link via --as-needed and isn't needed at runtime. (The cluster
# control plane is plain WebSocket now — no protoc/codegen needed.)
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libpq-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY pgmq ./pgmq
COPY muxa ./muxa
COPY katago-ws ./katago-ws

WORKDIR /build/katago-ws
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/katago-ws/target \
    cargo build --release --locked \
    && cp target/release/katago-ws /usr/local/bin/katago-ws

###############################################################################
# Stage 2 — fetch the KataGo engine, a model, and a config (no stripping)
###############################################################################
FROM debian:bookworm-slim AS katago
ARG KATAGO_VERSION
ARG KATAGO_ZIP
ARG KATAGO_BUILD_BACKEND
ARG MODEL_URL
ARG CONFIG_URL
ARG RUNTIME_APT_KEY_URL

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl unzip ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The engine lands in /kata/bin/, left empty when katago-src builds it instead.
WORKDIR /kata
RUN set -eu; mkdir -p /kata/bin /kata/keyrings; \
    if [ -z "${KATAGO_BUILD_BACKEND}" ]; then \
      curl -fsSL -o katago.zip \
        "https://github.com/lightvector/KataGo/releases/download/${KATAGO_VERSION}/${KATAGO_ZIP}"; \
      unzip -o katago.zip; \
      find . -type f -name katago -exec cp {} /kata/bin/katago \; ; \
      chmod +x /kata/bin/katago; \
      rm -f katago.zip; \
    fi; \
    curl -fsSL -o /kata/model.bin.gz "${MODEL_URL}"; \
    curl -fsSL -o /kata/analysis.cfg "${CONFIG_URL}"; \
    sed -i 's#^logDir.*#logDir = /tmp/katago-logs#' /kata/analysis.cfg; \
    if [ -n "${RUNTIME_APT_KEY_URL}" ]; then \
      curl -fsSL -o /kata/keyrings/runtime-repo.asc "${RUNTIME_APT_KEY_URL}"; \
    fi

###############################################################################
# Stage 2b — build KataGo from source (KATAGO_BUILD_BACKEND)
###############################################################################
# A no-op leaving /out/bin/ empty unless KATAGO_BUILD_BACKEND is set. The rocm /
# rocm-hip images build on ubuntu:24.04 (KATAGO_BUILD_BASE), like their runtimes.
#  - OPENCL (rocm image): v1.16.5 + the per-handle command queue patch, ~2.2x
#    the positions/s of the release binary on an RX 6800 XT (patches/README.md).
#  - ROCM (rocm-hip image): KataGo master's native MIOpen + hipBLAS backend,
#    built against AMD's ROCm dev packages (RUNTIME_APT_REPO) for HIP_ARCHS.
# AMD's repo is pinned above Debian's own (older) ROCm packages, as AMD's install
# guide does: Ubuntu universe's hipcc/rocm-cmake carry higher version strings.
FROM ${KATAGO_BUILD_BASE} AS katago-src
ARG KATAGO_SRC_REF
ARG KATAGO_BUILD_BACKEND
ARG KATAGO_BUILD_PKGS
ARG RUNTIME_APT_REPO
ARG HIP_ARCHS

COPY --from=katago /kata/keyrings/ /etc/apt/keyrings/
COPY katago-ws/patches/ /patches/
RUN set -eu; mkdir -p /out/bin; \
    if [ -n "${KATAGO_BUILD_BACKEND}" ]; then \
      apt-get update; \
      if [ -n "${RUNTIME_APT_REPO}" ]; then \
        apt-get install -y --no-install-recommends ca-certificates; \
        echo "deb [signed-by=/etc/apt/keyrings/runtime-repo.asc] ${RUNTIME_APT_REPO}" \
          > /etc/apt/sources.list.d/runtime-repo.list; \
        printf 'Package: *\nPin: origin %s\nPin-Priority: 600\n' \
          "$(echo ${RUNTIME_APT_REPO} | cut -d/ -f3)" > /etc/apt/preferences.d/runtime-repo; \
        apt-get update; \
      fi; \
      apt-get install -y --no-install-recommends ${KATAGO_BUILD_PKGS}; \
      git init -q /src; \
      git -C /src fetch -q --depth 1 https://github.com/lightvector/KataGo.git "${KATAGO_SRC_REF}"; \
      git -C /src checkout -q FETCH_HEAD; \
      for patch in /patches/katago-${KATAGO_SRC_REF}-*.patch; do \
        if [ -e "$patch" ]; then git -C /src apply -v "$patch"; fi; \
      done; \
      set --; \
      if [ -n "${HIP_ARCHS}" ]; then \
        set -- "-DCMAKE_HIP_ARCHITECTURES=$(echo ${HIP_ARCHS} | tr ' ' ';')"; \
      fi; \
      cmake -S /src/cpp -B /src/build -G Ninja \
        -DUSE_BACKEND="${KATAGO_BUILD_BACKEND}" -DCMAKE_BUILD_TYPE=Release "$@"; \
      cmake --build /src/build; \
      cp /src/build/katago /out/bin/katago; \
      # Only /out/bin leaves this stage: drop the ROCm toolchain (~13 GB) in
      # this same layer so it's never committed to the build cache.
      rm -rf /src /var/lib/apt/lists/* /opt/rocm*; \
    fi

###############################################################################
# Stage 3 — runtime base (everything but the katago-ws binary; base varies per
# platform). Split from the binary so it can be finished two ways below: by
# copying the just-compiled binary out of `build-binary` (stage `runtime`,
# used by local/`just` builds and the main-push image build), or by copying in
# a binary compiled once on the runner outside Docker (stage
# `runtime-prebuilt`, used by the tag-triggered release build so the slow
# LTO=fat compile isn't repeated per variant).
###############################################################################
FROM ${RUNTIME_BASE} AS runtime-base
ARG RUNTIME_PKGS
ARG RUSTICL_DRIVERS=none
ARG RUNTIME_APT_REPO
ARG HIP_ARCHS

# With RUNTIME_APT_REPO set, the repo is added (signed by the key fetched in the
# katago stage, pinned as in katago-src) before installing RUNTIME_PKGS.
# With HIP_ARCHS set, the ROCm math libraries' per-arch kernel files (rocBLAS /
# hipBLASLt Tensile libraries, MIOpen kernel DBs: names carry a gfxNNN token)
# are deleted for every arch not listed — in this same layer, or they'd still
# ship. No real gfx arch name is a prefix of another, so prefix matching also
# keeps MIOpen's "<arch><CUs>" DB names.
COPY --from=katago /kata/keyrings/ /etc/apt/keyrings/
RUN set -eu; apt-get update; \
    if [ -n "${RUNTIME_APT_REPO}" ]; then \
      apt-get install -y --no-install-recommends ca-certificates; \
      echo "deb [signed-by=/etc/apt/keyrings/runtime-repo.asc] ${RUNTIME_APT_REPO}" \
        > /etc/apt/sources.list.d/runtime-repo.list; \
      printf 'Package: *\nPin: origin %s\nPin-Priority: 600\n' \
        "$(echo ${RUNTIME_APT_REPO} | cut -d/ -f3)" > /etc/apt/preferences.d/runtime-repo; \
      apt-get update; \
    fi; \
    apt-get install -y --no-install-recommends ${RUNTIME_PKGS}; \
    rm -rf /var/lib/apt/lists/*; \
    if [ -n "${HIP_ARCHS}" ]; then \
      find /opt/rocm*/lib/rocblas/library /opt/rocm*/lib/hipblaslt/library \
           /opt/rocm*/share/miopen/db -type f -name '*gfx*' 2>/dev/null \
        | while read -r file; do \
            token=$(basename "$file" | grep -o 'gfx[0-9a-z]*' | head -n 1); \
            for arch in ${HIP_ARCHS}; do \
              case "$token" in "$arch"*) continue 2 ;; esac; \
            done; \
            rm -f "$file"; \
          done; \
      rm -rf /opt/rocm*/share/doc; \
    fi; \
    useradd --create-home --uid 10001 app; \
    # Mesa ships two OpenCL implementations and enumerates the deprecated one
    # (Clover) first, so KataGo's "device 0" lands on it. On RDNA2+ Clover
    # either can't build kernels at all or loses the GPU context mid-tune. Drop
    # its ICD so rusticl is the only Mesa OpenCL on offer; rusticl covers the
    # same gallium drivers, GCN onward. A no-op on the images without Mesa.
    rm -f /etc/OpenCL/vendors/mesa.icd

# Exactly one of these two holds the engine (the other dir is empty).
COPY --from=katago     /kata/bin/         /opt/katago/
COPY --from=katago-src /out/bin/          /opt/katago/
COPY --from=katago     /kata/model.bin.gz /opt/katago/model.bin.gz
COPY --from=katago     /kata/analysis.cfg /opt/katago/analysis.cfg
COPY katago-ws/config.toml                /app/config.toml

# Fail the build, not the first job, if the engine is missing or a source
# build's shared libs aren't in RUNTIME_PKGS (the AppImages bundle theirs).
RUN set -eu; test -x /opt/katago/katago; \
    if ldd /opt/katago/katago 2>/dev/null | grep 'not found'; then exit 1; fi

WORKDIR /app

# Release KataGo ships as an AppImage (source builds are plain ELFs, for which
# this is a no-op); the slim runtime has no FUSE, so
# APPIMAGE_EXTRACT_AND_RUN makes it extract-and-run instead of self-mounting
# (else "Cannot mount AppImage"). Baked in so every engine role (standalone,
# worker) works out of the box — no per-deploy env needed.
# rusticl only exposes a driver it has been asked for by name. `none` on the
# CPU/CUDA images, where there is no rusticl to configure.
ENV RUSTICL_ENABLE=${RUSTICL_DRIVERS} \
    KATAGO_WS_CONFIG=/app/config.toml \
    KATAGO_WS_WEB__HOST=0.0.0.0 \
    KATAGO_WS_WEB__PORT=3000 \
    KATAGO_WS_ENGINE__BINARY=/opt/katago/katago \
    KATAGO_WS_ENGINE__CONFIG=/opt/katago/analysis.cfg \
    KATAGO_WS_ENGINE__MODEL=/opt/katago/model.bin.gz \
    APPIMAGE_EXTRACT_AND_RUN=1 \
    RUST_LOG=info

EXPOSE 3000

# The binary probes its own GET /healthz on [web].port (`katago-ws healthcheck`),
# so the image needs no curl/wget. Every role that serves HTTP answers /healthz.
# The long start period covers a worker's first start, which launches KataGo
# (model load, and on a new GPU OpenCL kernel tuning) before /healthz comes up;
# failures inside it don't count, and the first success marks it healthy.
# Inherited by both leaf stages below.
HEALTHCHECK --interval=15s --timeout=5s --start-period=600s --retries=3 \
    CMD ["katago-ws", "healthcheck"]

# USER is deliberately NOT set here — it comes last in each leaf stage below,
# after that stage's binary COPY, so every COPY in this Dockerfile still runs
# as root (matching the pre-split behavior) and only the final image runs
# unprivileged.

# Release target: binary supplied via a named Buildx build context (`prebuilt`)
# instead of compiled here — see .github/workflows/release.yml. Deliberately
# defined BEFORE `runtime` below so `runtime` stays the last stage in the file
# and is still what gets built by anything that invokes `docker build` without
# an explicit --target.
FROM runtime-base AS runtime-prebuilt
COPY --from=prebuilt /katago-ws /usr/local/bin/katago-ws
USER app
ENTRYPOINT ["katago-ws"]

# Default target: binary compiled in this same Docker build (local/`just`,
# main-push image.yml). Must stay the LAST stage — see note above.
FROM runtime-base AS runtime
COPY --from=build-binary /usr/local/bin/katago-ws /usr/local/bin/katago-ws
USER app
ENTRYPOINT ["katago-ws"]
