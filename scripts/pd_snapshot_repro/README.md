# A100 snapshot-serving reproduction

These scripts reproduce the validated Qwen3-4B single-instance and two-GPU
NIXL PD setup on `ai_hgx_03`. They refuse to start if the selected GPU already
has a compute process or if a required port is occupied.

Environment: `/storage/data/yingxy2024/envs/sglang-pd-repro`

Run a single server:

```bash
scripts/pd_snapshot_repro/run_single_a100.sh
```

Run the full prefill/decode smoke test (defaults to GPU 6 and 7):

```bash
scripts/pd_snapshot_repro/smoke_a100_nixl_pd.sh
```

GPU IDs, ports, model path, version, IB device, log directory, and environment
path can be overridden with the uppercase variables used at the top of each
script. The smoke script terminates only the three processes it starts.

`run_gateway.sh` uses the environment's installed `sglang-router==0.3.2`, which
is the Gateway used in the recorded GPU validation. The Rust Gateway changes
in this repository are covered by the CPU tests documented under
`docs/developer_guide/pd_weight_version_selection_bugfix.md`.
