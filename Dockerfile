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
ARG CONFIG_URL=https://raw.githubusercontent.com/lightvector/KataGo/v1.16.5/cpp/configs/analysis_example.cfg
ARG RUNTIME_PKGS=ca-certificates

###############################################################################
# Stage 1 — build the Rust binary (glibc / Debian)
###############################################################################
FROM rust:1-bookworm AS builder

# pkg-config + libpq-dev let pq-sys (pulled transitively by diesel) link; the
# binary doesn't actually call libpq (diesel-async is pure Rust), so it's
# dropped at link via --as-needed and isn't needed at runtime.
# protobuf-compiler provides `protoc`, needed at build time by tonic-prost-build
# (build.rs) to compile proto/cluster.proto for the orchestrator/worker gRPC.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libpq-dev protobuf-compiler \
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
# Stage 3 — runtime (base varies per platform)
###############################################################################
FROM ${RUNTIME_BASE} AS runtime
ARG RUNTIME_PKGS

RUN apt-get update && apt-get install -y --no-install-recommends ${RUNTIME_PKGS} \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 app

COPY --from=builder /usr/local/bin/katago-ws /usr/local/bin/katago-ws
COPY --from=katago /kata/katago           /opt/katago/katago
COPY --from=katago /kata/model.bin.gz     /opt/katago/model.bin.gz
COPY --from=katago /kata/analysis.cfg     /opt/katago/analysis.cfg
COPY katago-ws/muxa.toml             /app/muxa.toml

WORKDIR /app
USER app

# KataGo ships as an AppImage; the slim runtime has no FUSE, so
# APPIMAGE_EXTRACT_AND_RUN makes it extract-and-run instead of self-mounting
# (else "Cannot mount AppImage"). Baked in so every engine role (standalone,
# worker) works out of the box — no per-deploy env needed.
ENV MUXA_CONFIG=/app/muxa.toml \
    MUXA_WEB__HOST=0.0.0.0 \
    MUXA_WEB__PORT=3000 \
    MUXA_ENGINE__BINARY=/opt/katago/katago \
    MUXA_ENGINE__CONFIG=/opt/katago/analysis.cfg \
    MUXA_ENGINE__MODEL=/opt/katago/model.bin.gz \
    APPIMAGE_EXTRACT_AND_RUN=1 \
    RUST_LOG=info

EXPOSE 3000

# No in-image HEALTHCHECK tool; probe GET /health from your orchestrator/LB.
ENTRYPOINT ["katago-ws"]
