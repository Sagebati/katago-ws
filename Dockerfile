# katago-ws — per-platform deployable image.
#
# One Dockerfile, parameterized by build-args, builds the CPU / Vulkan / ROCm
# images of katago-ws with its engine, katago-rs (KataGo's analysis engine in
# Rust; the sibling `katago-rs` repo). See the justfile for the ready-made
# targets, or:
#
#   docker build -f katago-ws/Dockerfile -t katago-ws:cpu .     # from the PARENT dir
#
# katago-rs compiles its networks in from ONNX (its tools/bin2onnx converts
# KataGo's .bin.gz) and loads their weights from the .bpk files the build
# writes; both happen here, from the nets' public URLs.
#
# Build context must be the PARENT directory (contains katago-ws, katago-rs).
# muxa and pgmq are git deps, fetched by cargo at their Cargo.lock revs.

# syntax=docker/dockerfile:1

ARG RUNTIME_BASE=debian:bookworm-slim
ARG RUNTIME_PKGS=ca-certificates
# Optional extra apt repo for the runtime stage (AMD's ROCm repo): an
# ASCII-armored signing key URL and the rest of a `deb` line.
ARG RUNTIME_APT_GPG_URL=
ARG RUNTIME_APT_REPO=
# katago-rs's cargo features: one backend (flex, wgpu, rocm) and the nets.
ARG ENGINE_FEATURES=flex,net-b10c128
# The engine's [engine] backend / fp16 / model for this image.
ARG ENGINE_BACKEND=flex
ARG ENGINE_FP16=false
ARG ENGINE_MODEL=g170e-b10c128.bpk
# The networks (KataGo .bin.gz), converted for whichever net-* features are on.
# b10c128 is the CPU image's net (b18 runs ~1 eval/s on a CPU); kata1 b18c384nbt
# the GPU images' (on katago-rs ROCm, RX 6800 XT: 13.4 positions/s at 100
# visits, 1.9x KataGo's OpenCL backend); b18c384nbt-humanv0 the human-SL net.
ARG NET_B10_URL=https://katagoarchive.org/g170/neuralnets/g170e-b10c128-s1141046784-d204142634.bin.gz
ARG NET_B18_URL=https://media.katagotraining.org/uploaded/networks/models/kata1/kata1-b18c384nbt-s9996604416-d4316597426.bin.gz
ARG NET_HUMAN_URL=https://github.com/lightvector/KataGo/releases/download/v1.15.0/b18c384nbt-humanv0.bin.gz
ARG CONFIG_URL=https://raw.githubusercontent.com/lightvector/KataGo/v1.18.2/cpp/configs/analysis_example.cfg

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
COPY katago-ws ./katago-ws

WORKDIR /build/katago-ws
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/katago-ws/target \
    cargo build --release --locked \
    && cp target/release/katago-ws /usr/local/bin/katago-ws

###############################################################################
# Stage 2 — the networks as ONNX (katago-rs's tools/bin2onnx), and the config
###############################################################################
FROM python:3.14-slim-bookworm AS nets
ARG ENGINE_FEATURES
ARG NET_B10_URL
ARG NET_B18_URL
ARG NET_HUMAN_URL
ARG CONFIG_URL
ARG RUNTIME_APT_GPG_URL

RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir numpy==2.5.4 onnx==1.23.2

COPY katago-rs/tools/bin2onnx/bin2onnx.py /bin2onnx.py
# Only the nets this image compiles in (its net-* features).
RUN set -eu; mkdir -p /onnx /keyrings; \
    convert() { \
      curl -fsSL -o /tmp/net.bin.gz "$2"; \
      python -I /bin2onnx.py /tmp/net.bin.gz -o "/onnx/$1.onnx"; \
      rm -f /tmp/net.bin.gz; \
    }; \
    case ",${ENGINE_FEATURES}," in *,net-b10c128,*) convert g170e-b10c128 "${NET_B10_URL}";; esac; \
    case ",${ENGINE_FEATURES}," in *,net-b18c384nbt,*) convert kata1-b18c384nbt "${NET_B18_URL}";; esac; \
    case ",${ENGINE_FEATURES}," in *,net-b18c384nbt-humanv0,*) convert b18c384nbt-humanv0 "${NET_HUMAN_URL}";; esac; \
    curl -fsSL -o /analysis.cfg "${CONFIG_URL}"; \
    sed -i 's#^logDir.*#logDir = /tmp/katago-logs#' /analysis.cfg; \
    if [ -n "${RUNTIME_APT_GPG_URL}" ]; then \
      curl -fsSL -o /keyrings/runtime-repo.asc "${RUNTIME_APT_GPG_URL}"; \
    fi

###############################################################################
# Stage 3 — build katago-rs's analysis binary, and collect the nets' .bpk
###############################################################################
# katago-rs has no runtime GPU SDK dependency at build time: the ROCm backend
# (cubecl-hip) ships pre-generated HIP bindings and dlopens the HIP runtime,
# and wgpu dlopens Vulkan. cubecl-hip downloads a prebuilt LLVM once (cached).
FROM rust:1.99.1-bookworm AS engine-build
ARG ENGINE_FEATURES

RUN apt-get update && apt-get install -y --no-install-recommends jq \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY katago-rs ./katago-rs
COPY --from=nets /onnx/ ./katago-rs/models/onnx/

WORKDIR /build/katago-rs
# The .bpk files are written to katago-nn-burn's build-script out dir; its path
# is read from cargo's own messages (the target dir is a cache mount, so both
# the binary and the nets are copied out in this same step).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/katago-rs/target \
    --mount=type=cache,target=/root/.cache/tracel \
    set -eu; \
    cargo build --release --locked -p katago-cli --no-default-features -F "${ENGINE_FEATURES}" \
      --message-format=json-render-diagnostics > /tmp/cargo.json; \
    out="$(jq -r 'select(.reason=="build-script-executed" and (.package_id|test("katago-nn-burn"))) | .out_dir' /tmp/cargo.json | tail -1)"; \
    mkdir -p /out/nets; \
    cp target/release/katago-rs /out/katago-rs; \
    cp "$out"/model/*.bpk /out/nets/; \
    ls -la /out /out/nets

###############################################################################
# Stage 4 — runtime base (everything but the katago-ws binary; base varies per
# platform). Split from the binary so it can be finished two ways below: by
# copying the just-compiled binary out of `build-binary` (stage `runtime`,
# used by local/`just` builds and the main-push image build), or by copying in
# a binary compiled once on the runner outside Docker (stage
# `runtime-prebuilt`, used by the tag-triggered release build so the slow
# LTO=fat compile isn't repeated per variant).
###############################################################################
FROM ${RUNTIME_BASE} AS runtime-base
ARG RUNTIME_PKGS
ARG RUNTIME_APT_REPO
ARG ENGINE_BACKEND
ARG ENGINE_FP16
ARG ENGINE_MODEL

# With RUNTIME_APT_REPO set, the repo is added (signed by the key fetched in the
# nets stage) and pinned above the distro's own packages, as AMD's install guide
# does, before installing RUNTIME_PKGS.
COPY --from=nets /keyrings/ /etc/apt/keyrings/
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
    rm -rf /var/lib/apt/lists/* /opt/rocm*/share/doc; \
    useradd --create-home --uid 10001 app; \
    # The katago-ws auto-tune cache lives here; created now so a volume mounted
    # on it starts out owned by `app`.
    install -d -o app -g app /home/app/.katago

COPY --from=engine-build /out/katago-rs   /opt/katago-rs/katago-rs
COPY --from=engine-build /out/nets/       /opt/katago-rs/nets/
COPY --from=nets         /analysis.cfg    /opt/katago-rs/analysis.cfg
COPY katago-ws/config.toml                /app/config.toml

# Fail the build, not the first job, if the engine is missing or its linked
# libraries aren't in RUNTIME_PKGS (GPU runtimes are dlopened at run time).
RUN set -eu; test -x /opt/katago-rs/katago-rs; \
    if ldd /opt/katago-rs/katago-rs 2>/dev/null | grep 'not found'; then exit 1; fi; \
    /opt/katago-rs/katago-rs version

WORKDIR /app

ENV KATAGO_WS_CONFIG=/app/config.toml \
    KATAGO_WS_WEB__HOST=0.0.0.0 \
    KATAGO_WS_WEB__PORT=3000 \
    KATAGO_WS_ENGINE__BINARY=/opt/katago-rs/katago-rs \
    KATAGO_WS_ENGINE__BACKEND=${ENGINE_BACKEND} \
    KATAGO_WS_ENGINE__FP16=${ENGINE_FP16} \
    KATAGO_WS_ENGINE__CONFIG=/opt/katago-rs/analysis.cfg \
    KATAGO_WS_ENGINE__MODEL=/opt/katago-rs/nets/${ENGINE_MODEL} \
    # rocm: where cubecl-hip finds the HIP runtime it dlopens.
    ROCM_PATH=/opt/rocm \
    # vulkan on NVIDIA: the container toolkit mounts the Vulkan driver only for
    # the `graphics` capability.
    NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics \
    RUST_LOG=info

EXPOSE 3000

# The binary probes its own GET /healthz on [web].port (`katago-ws healthcheck`),
# so the image needs no curl/wget. Every role that serves HTTP answers /healthz.
# The long start period covers a worker's first start, which launches the
# engine (model load, and on a new GPU the auto-tune and the first-use kernel
# compilation) before /healthz comes up; failures inside it don't count, and
# the first success marks it healthy. Inherited by both leaf stages below.
HEALTHCHECK --interval=15s --timeout=5s --start-period=600s --retries=3 \
    CMD ["katago-ws", "healthcheck"]

# USER is deliberately NOT set here — it comes last in each leaf stage below,
# after that stage's binary COPY, so every COPY in this Dockerfile still runs
# as root and only the final image runs unprivileged.

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
