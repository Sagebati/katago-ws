# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases from this point on are cut by [release-plz](https://release-plz.dev) from
Conventional Commit PR titles; entries below this point summarize history prior to
that automation.

## [0.1.0] - 2026-08-03

Initial tagged release. Queued SGF analysis service (`POST /analyse` → KataGo worker
→ `GET /analyse/{id}`), `standalone`/`orchestrator`/`worker` launch roles, cpu/cuda/opencl
container images published to `ghcr.io/sagebati/katago-ws`.
