# KataGo patches

Patches applied to KataGo when the `opencl` and `rocm` images build it from
source (the `katago-src` stage of the [Dockerfile](../Dockerfile), enabled by
`KATAGO_BUILD_BACKEND=OPENCL`). The stage clones KataGo at `KATAGO_VERSION` and
`git apply`s every `katago-${KATAGO_VERSION}-*.patch` here, so a version bump
needs the patches rebased and renamed (or the build fails at `git apply`). The
`cpu` and `cuda` images still use the release zips; the patch touches only
`cpp/neuralnet/openclbackend.cpp`, so it would change nothing there.

## `katago-v1.16.5-opencl-per-handle-queue.patch`

KataGo's OpenCL backend creates one in-order command queue per device
(`DevicesContext` in `openclhelpers.cpp`) and every `ComputeHandle` uses it, so
all NN server threads in a process serialize their batches, and their blocking
reads, on that one queue. The GPU sits idle between them: on an RX 6800 XT, ~55%
busy with the b10 net and ~87% with b20, however many server threads are set.
The patch gives each handle its own `clCreateCommandQueue` on the shared context
(released in the destructor). Each handle already owns its weights, scratch
buffers and kernel objects, so nothing else needs synchronizing.

Results with `katago-ws bench` (17-game corpus, 100 visits,
`numAnalysisThreads=256,numSearchThreadsPerAnalysisThread=1`; RX 6800 XT, ROCm
OpenCL):

| Net     | Stock (4 srv threads, batch 64) | Patched, 4 srv / batch 64 | Patched, 8 srv / batch 32 | Stock, 2 processes |
|---------|------:|------:|------:|------:|
| b10c128 |  75.7 | 147.1 | **168.4** | 101 |
| b20c256 |  26.3 |  33.5 |  **35.3** |  30 |

positions/s. With the patch the GPU runs ~98-99% busy. `katago testgpuerror`
statistics are identical, and per-move evaluations differ from stock only at the
MCTS noise floor.

Deployment setting (one engine per GPU; see `deploy/split/worker.compose.yml`):

```
MUXA_ENGINE__OVERRIDE_CONFIG="numAnalysisThreads=256,numSearchThreadsPerAnalysisThread=1,nnMaxBatchSize=32,numNNServerThreadsPerModel=8"
```

### Draft upstream PR (not opened)

Kept here for you to decide whether to send it to lightvector/KataGo. Before
sending: check it against current `master` (the patch is against v1.16.5), and
consider measuring on an NVIDIA or Intel OpenCL driver too, since the numbers
below come from one AMD card.

> **OpenCL: give each compute handle its own command queue**
>
> The OpenCL backend shares one in-order `cl_command_queue` per device across
> every `ComputeHandle`, i.e. across all NN server threads that use that device.
> Their kernels and blocking `clEnqueueReadBuffer`s serialize on it, so raising
> `numNNServerThreadsPerModel` adds no GPU concurrency: one thread's host-side
> work (input packing, waiting on the read, output handling) leaves the GPU
> idle instead of overlapping with another thread's batch.
>
> This change creates a queue per `ComputeHandleInternal` on the device's
> existing context (with `CL_QUEUE_PROFILING_ENABLE` when live kernel profiling
> is on, as before) and releases it in the destructor. Each handle already has
> its own buffers, scratch space and kernel objects; the programs and weights
> it shares are read-only after setup, so no further synchronization is needed.
> The device-level queue in `DevicesContext` is left in place (nothing else in
> the backend uses it) to keep the change small; a follow-up could drop it.
>
> Measured with the analysis engine on an RX 6800 XT (ROCm 7.2 OpenCL), 100
> visits, 256 analysis threads x 1 search thread, positions/s:
>
> | Net | Before (4 server threads, batch 64) | After (4, batch 64) | After (8, batch 32) |
> |---|---:|---:|---:|
> | g170e-b10c128 | 75.7 | 147.1 | 168.4 |
> | g170e-b20c256x2 | 26.3 | 33.5 | 35.3 |
>
> GPU utilization goes from ~55% (b10) / ~87% (b20) to ~98%. It also beats
> running two separate KataGo processes on the same GPU (b10 101, b20 30
> positions/s). `katago testgpuerror` output is identical before and after.
> Small nets benefit most, since their per-batch host overhead is a larger share
> of each batch.
