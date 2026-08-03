# katago-ws — per-platform deployable image.
#
# One Dockerfile, parameterized by build-args, builds a CPU / CUDA / OpenCL(AMD)
# image. See the Makefile for the three ready-made targets, or:
#
#   docker build -f katago-ws/Dockerfile -t katago-ws:cpu .     # from the PARENT dir
#
# The Rust binary is a standard glibc (Debian) build. KataGo's CPU build is
# statically linked; its GPU builds dynamically link the vendor runtime supplied
# by the base image.
#
# Build context must be the PARENT directory (contains katago-ws, muxa, pgmq).

# syntax=docker/dockerfile:1

ARG RUNTIME_BASE=debian:bookworm-slim
ARG KATAGO_VERSION=v1.16.5
ARG KATAGO_ZIP=katago-v1.16.5-eigen-linux-x64.zip
ARG MODEL_URL=https://github.com/lightvector/KataGo/releases/download/v1.4.5/g170e-b20c256x2-s5303129600-d1228401921.bin.gz
ARG CONFIG_URL=https://raw.githubusercontent.com/lightvector/KataGo/${KATAGO_VERSION}/cpp/configs/analysis_example.cfg
ARG RUNTIME_PKGS=ca-certificates

###############################################################################
# Stage 1 — build the Rust binary (glibc / Debian)
#
# Used by the local/`just` path and by the main-push image build, both of
# which compile in-container. The tag-triggered release build compiles once
# on the runner instead and skips straight to `runtime-prebuilt` below, so
# the (slow, LTO) compile isn't repeated per variant.
###############################################################################
FROM rust:1.90.0-bookworm AS build-binary

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
ARG MODEL_URL
ARG CONFIG_URL

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl unzip ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /kata
RUN curl -fsSL -o katago.zip \
        "https://github.com/lightvector/KataGo/releases/download/${KATAGO_VERSION}/${KATAGO_ZIP}" \
    && unzip -o katago.zip \
    && find . -type f -name katago -exec cp {} /kata/katago \; \
    && chmod +x /kata/katago \
    && curl -fsSL -o /kata/model.bin.gz "${MODEL_URL}" \
    && curl -fsSL -o /kata/analysis.cfg "${CONFIG_URL}" \
    && sed -i 's#^logDir.*#logDir = /tmp/katago-logs#' /kata/analysis.cfg \
    && rm -f katago.zip

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

RUN apt-get update && apt-get install -y --no-install-recommends ${RUNTIME_PKGS} \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 app \
    # Mesa ships two OpenCL implementations and enumerates the deprecated one
    # (Clover) first, so KataGo's "device 0" lands on it. On RDNA2+ Clover
    # either can't build kernels at all or loses the GPU context mid-tune. Drop
    # its ICD so rusticl is the only Mesa OpenCL on offer; rusticl covers the
    # same gallium drivers, GCN onward. A no-op on the CPU/CUDA images, which
    # have no Mesa.
    && rm -f /etc/OpenCL/vendors/mesa.icd

COPY --from=katago /kata/katago           /opt/katago/katago
COPY --from=katago /kata/model.bin.gz     /opt/katago/model.bin.gz
COPY --from=katago /kata/analysis.cfg     /opt/katago/analysis.cfg
COPY katago-ws/muxa.toml             /app/muxa.toml

WORKDIR /app

# KataGo ships as an AppImage; the slim runtime has no FUSE, so
# APPIMAGE_EXTRACT_AND_RUN makes it extract-and-run instead of self-mounting
# (else "Cannot mount AppImage"). Baked in so every engine role (standalone,
# worker) works out of the box — no per-deploy env needed.
# rusticl only exposes a driver it has been asked for by name. `none` on the
# CPU/CUDA images, where there is no rusticl to configure.
ENV RUSTICL_ENABLE=${RUSTICL_DRIVERS} \
    MUXA_CONFIG=/app/muxa.toml \
    MUXA_WEB__HOST=0.0.0.0 \
    MUXA_WEB__PORT=3000 \
    MUXA_ENGINE__BINARY=/opt/katago/katago \
    MUXA_ENGINE__CONFIG=/opt/katago/analysis.cfg \
    MUXA_ENGINE__MODEL=/opt/katago/model.bin.gz \
    APPIMAGE_EXTRACT_AND_RUN=1 \
    RUST_LOG=info

EXPOSE 3000

# No in-image HEALTHCHECK tool; probe GET /health from your orchestrator/LB.
# USER is deliberately NOT set here — it comes last in each leaf stage below,
# after that stage's binary COPY, so every COPY in this Dockerfile still runs
# as root (matching the pre-split behavior) and only the final image runs
# unprivileged.

# Release target: binary supplied via a named Buildx build context (`prebuilt`)
# instead of compiled here — see .github/workflows/release.yml. Deliberately
# defined BEFORE `runtime` below so `runtime` stays the last stage in the file
# and is still what gets built by anything that invokes `docker build` without
# an explicit --target (e.g. Cloudflare's wrangler.jsonc container build).
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
