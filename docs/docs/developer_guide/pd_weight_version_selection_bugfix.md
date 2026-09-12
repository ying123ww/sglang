# Snapshot-serving A100 environment and PD weight-version report

## Outcome

The clean repository at commit `0b3bb0cbe31873994c9f989fddfe2f87ca839fdd`
(`v0.5.15.post1`) now has an independent CUDA 12.9 environment and a validated
A100 execution path. Qwen3-4B passed both a TP=1 greedy/logprob request and a
real two-GPU prefill/decode request using NIXL/UCX KV transfer.

In strict mode, the Gateway enforces compatible P/D model identity and
`weight_version` metadata on both gRPC and HTTP paths, and pins the chosen
version across retries. Legacy mode preserves upstream routing behavior.
This closes the known Gateway selection and retry
drift bugs. It does not yet provide a worker-side transactional snapshot
admission protocol; that remaining boundary is described below.

## A100 source-environment investigation

Read-only inspection of `ai_hgx_03` found:

- eight `NVIDIA A100-SXM4-80GB` GPUs (81920 MiB each);
- NVIDIA driver `550.90.07`;
- Linux `3.10.0-957.el7.x86_64`, x86_64, glibc `2.31`;
- CUDA compiler and `ptxas` from the `slime` environment at CUDA `12.9.86`;
- Python `3.12.13`, torch `2.11.0+cu129`, FlashInfer Python/cubin `0.6.12`,
  `sglang-kernel 0.4.5+cu129`, and `sgl-deep-gemm 0.1.4+cu129`.

The old environment is not a clean upstream baseline. Its editable SGLang
installation reports version `0.0.0` and points to:

```text
/public/home/yingxy2024/moe-consistency/sglang/python
```

The existing launcher
`moe-consistency/experiments/run_sglang_qwen3_a100_deepep_triton.sh` overrides
that editable install again through `PYTHONPATH`, loading:

```text
/public/home/yingxy2024/moe-consistency/experiments/sglang-main-sm80/python
```

It is configured for a different model (`Qwen3-30B-A3B-2layer`), GPUs 4-7,
TP=4, EP=4, DeepEP, Triton attention, and PyTorch sampling. No log containing a
server-ready or inference-batch success marker was found under that experiment
directory. Consequently, the old evidence proves package installation and
import only; the script's existence is not treated as a successful run.

## Target selection and independent environment

The first validated target is `ai_hgx_03`, not the L20 host. This keeps the
known CUDA 12.9 package family and the A100 architecture while removing the
editable-source mixture. It avoids changing hardware and software at the same
time. The shared environment has not been accepted on L20; an L20 run remains
a separate sm89 migration test with its own JIT cache.

Environment:

```text
/storage/data/yingxy2024/envs/sglang-pd-repro
```

Verified core versions:

| Component | Installed version/source |
| --- | --- |
| Python | 3.12.13 |
| torch | 2.11.0+cu129, inherited from the working CUDA 12.9 package stack |
| CUDA toolkit/nvcc/ptxas | 12.9.86 |
| flashinfer-python | 0.6.12, inherited CUDA 12 stack |
| flashinfer-cubin | 0.6.12, inherited CUDA 12 stack |
| sglang-kernel | 0.4.4+cu129, local `sglang_kernel-0.4.4+cu129-cp310-abi3-manylinux2014_x86_64.whl` |
| sglang-kernel wheel SHA256 | `4ea2f7176965c3d1a5697254befe45c2ac995793577108129710d0f66ca85b33` |
| sgl-deep-gemm | 0.1.4+cu129, inherited but not used by this validation |
| nixl / nixl-cu12 | 1.3.0 / 1.3.0 |
| sglang-router | 0.3.2 |
| sglang | editable from this repository's `python/` directory, installed without dependency resolution |

The kernel wheel came from
`/public/home/yingxy2024/.tmp-codex/moe-a100-wheels/`. Its directory name was
not used as compatibility evidence: the final evidence is the successful sm80
JIT, kernel load, model execution, and PD request.

Intentional or inherited deviations from the repository declaration are:

- CUDA 12.9 (`cuda-python 12.9.0` and CUDA-12 FlashInfer artifacts) instead of
  the declared CUDA 13 family;
- `sglang-kernel 0.4.4+cu129`, matching the repository's 0.4.4 API version but
  using the CUDA 12.9 build suffix;
- inherited cuDNN `9.16.0.29` instead of torch's declared `9.17.1.4`;
- no `flash-attn-4`; it is not used by the validated FlashInfer path;
- inherited NumPy `1.26.4` versus a newer optional `tifffile` requirement;
- an unsupported-platform `transformer-engine` marker.

`pip install --no-deps -e` was used only to prevent dependency replacement.
Compatibility is supported by the actual GPU tests below, not by that install
command.

### Why Mooncake is not required here

`mooncake-transfer-engine==0.3.11.post1` exists on PyPI, but its Linux x86_64
wheel requires glibc 2.35. Both target hosts have glibc 2.31 and the release
has no source distribution, so pip has no compatible artifact. SGLang's PD
path also supports NIXL; this reproduction uses the exact CUDA-12 pair
`nixl==1.3.0` and `nixl-cu12==1.3.0`. Both P and D initialized the UCX backend
and completed a real request, so Mooncake is not necessary for this baseline.

The environment-local libnl SONAME links are kept ahead of system libnl to
avoid the NIXL wheel/system-library collision. Per-host cache directories keep
sm80 and sm89 CUDA, FlashInfer, Torch-extension, Triton, and TVM-FFI artifacts
separate. CUDA runtime library directories are also in `LIBRARY_PATH`, which
fixed FlashInfer JIT's final `-lcudart` link on the A100 host. `TMPDIR` points
to storage because the host's system `/tmp` was full; no system files were
removed.

## Gateway changes

### Selection and metadata

The original gRPC selector independently selected P and D workers and could
return `P(v10)+D(v11)`. Strict mode now filters by compatible identities before invoking
the existing load-balancing policies, so it can also find a valid matching
pair inside a mixed candidate pool.

The locked `smg-grpc-client 1.0.0` generated `GetModelInfoResponse` exposes
`weight_version` as a top-level field. The regression test constructs that
real response type and follows the production `to_labels()` chain into worker
metadata and selection. It does not use a hand-written label mock for this
metadata-chain test.

HTTP discovery now copies `/model_info`'s `weight_version` into worker labels,
and the strict HTTP PD selector applies the same constraint before its separate P/D
policies.

### Legacy and strict modes

The Gateway option is:

```text
--pd-weight-version-mode legacy|strict
```

`legacy` is the default and preserves original independent P/D selection,
including mismatches and missing versions. It does not bind retry versions.
This aligns with charter SEL-13; earlier commits used legacy for a more
restrictive behavior, which has now been corrected.

`strict` requires the same non-blank explicit version and rejects the literal
placeholder `default`. It requires equal, known canonical model IDs. If either
side publishes `model_path` or `deployment_scope`, both must publish the same
non-blank value. Without a request model, there must be one compatible
model/source/deployment identity. HTTP honors explicit request models in strict
mode even with `enable_igw=false`.

I-a supports a controlled registry with trusted registration and immutable
version labels. Missing scope on both sides means the registry's one deployment;
operators must assign scopes when sharing a registry across deployments.
Matching paths and labels are registration identities, not checkpoint hashes.

### Retry binding

In strict mode, HTTP and gRPC use a request-local shared `OnceLock<Option<String>>`.
The first successful pair selection binds the version. Every subsequent retry
filters candidates to that version, allowing a replacement worker at the same
version but preventing drift from (for example) v10 to v11.

The Bucket policy also verifies that its internally remembered URL remains in
the version-filtered candidate slice. If not, it falls back to a healthy
worker from the allowed slice rather than returning no worker.

## Validation evidence

### CPU/Gateway

The I-a closeout expands the original 16 focused tests to a 27-test contract
suite. It covers model/source/deployment identity, legacy semantics, separate
HTTP policies, filtered indices, hash and target headers, existing eligibility,
and actual loopback HTTP discovery through production worker registration.
The charter mapping and final run evidence are in [Phase I-a acceptance](phase_ia_acceptance.md).

### A100 TP=1 model

Model: `/storage/data/yingxy2024/Models/Qwen3-4B`; TP=1; FlashInfer attention
and sampling. FlashInfer compiled for sm80, weights loaded, CUDA graphs were
captured, and a greedy request with output logprobs succeeded.

Prompt and generated text:

```text
The capital of France is
 Paris. The capital of Germany is Berlin. The capital of
```

The response contains 12 output IDs and 12 output-token logprobs. Evidence:

```text
.local/logs/a100-single-qwen3-4b.log
.local/logs/a100-single-greedy-logprob-response.json
```

### A100 P/D with NIXL

P ran on GPU 6 at port 31001 with bootstrap port 39001; D ran on GPU 7 at
31002; Gateway ran at 31003. Both workers logged `NIXL KVManager initialized
with backend: UCX`, P started `CommonKVBootstrapServer` and processed a real
prefill batch, and D returned generation. The routed request produced the same
12 output IDs and numerically matching logprobs as the TP=1 request and reported
`weight_version="baseline"`.

Evidence:

```text
.local/logs/a100-p-nixl.log
.local/logs/a100-d-nixl.log
.local/logs/a100-pd-router.log
.local/logs/a100-pd-nixl-greedy-logprob-response.json
```

The GPU run used the environment's installed `sglang-router==0.3.2`, whose
module is
`.../site-packages/sglang_router/__init__.py`. Therefore the GPU result proves
the clean SGLang/CUDA/NIXL model-serving baseline. The branch's new Rust
Gateway selection logic is proven by the focused Rust tests, not by falsely
claiming the installed wheel already contains these unbuilt branch changes.

Reusable commands are tracked in `scripts/pd_snapshot_repro/`. The full smoke
script checks GPU and port availability, launches only its own P/D/Gateway
processes, validates the response and NIXL markers, and terminates only those
processes on exit.

## Remaining protocol boundary

A worker-side strict reader-lock recheck cannot be implemented honestly as a
small Gateway-only patch. This repository uses `smg-grpc-client 1.0.0`, while
the environment has `smg-grpc-servicer 0.8.0` and `smg-grpc-proto 0.4.14`.
Their generate/disaggregation request types carry bootstrap host, port, and
room but no `required_weight_version`. In addition, the current external SMG
gRPC request manager bypasses `TokenizerManager.model_update_lock`.

Closing that boundary requires one coordinated change:

1. add required snapshot identity to the SMG proto/client/servicer contract
   and check it while holding the model-update reader lock; or
2. finish migration to the repository's native gRPC bridge, which enters the
   tokenizer manager, then add the same field and check there.

The weight-update path should also publish `server_args.weight_version` inside
the same writer-lock critical section as the weight swap. Today there is a
small post-unlock metadata window. Until those changes land, this branch
guarantees Gateway selection/retry consistency for observed registration
metadata, not a full distributed transaction against an in-flight update.

Other residual operational risks are stale registration metadata and manual
labels overriding discovery. Strict selection rejects conflicting model,
source, or deployment identities even with matching versions, but cannot
prove that two arbitrary identical strings refer to cryptographically
identical weights.

## Incremental commits

```text
897ce89 test(gateway): reproduce PD weight version mismatch
1eb0346 fix(gateway): keep PD workers on one weight version
fab81ab docs: record PD version selection bugfix scope
903936a test(gateway): cover gRPC weight metadata chain
afa1376 feat(gateway): add strict PD weight selection
50e1c5d fix(gateway): enforce PD versions for HTTP routing
ef8ccea fix(gateway): pin PD version across retries
8bb548b fix(gateway): keep bucket selection within filtered workers
6febf29 chore(repro): add A100 NIXL PD smoke scripts
10994b7 fix(gateway): complete strict PD identity and legacy selection contract
51aec90 test(gateway): exercise HTTP discovery through strict PD selection
fa7f0ba fix(gateway): retain PD selector for production health checks
```
