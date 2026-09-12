#!/usr/bin/env bash
set -euo pipefail
source /public/home/yingxy2024/sglang-snapshot-serving/scripts/pd_snapshot_repro/common.sh

LOG_DIR=${LOG_DIR:-$SGLANG_PD_REPO/.local/logs}
PID_DIR=${PID_DIR:-$SGLANG_PD_REPO/.local/pids}
mkdir -p "$LOG_DIR" "$PID_DIR"

p_pid=
d_pid=
router_pid=
cleanup() {
  for pid in "$router_pid" "$p_pid" "$d_pid"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT INT TERM

"$SGLANG_PD_REPO/scripts/pd_snapshot_repro/run_prefill_a100_nixl.sh" \
  >"$LOG_DIR/a100-p-nixl.log" 2>&1 &
p_pid=$!
echo "$p_pid" >"$PID_DIR/a100-p-nixl.pid"
"$SGLANG_PD_REPO/scripts/pd_snapshot_repro/run_decode_a100_nixl.sh" \
  >"$LOG_DIR/a100-d-nixl.log" 2>&1 &
d_pid=$!
echo "$d_pid" >"$PID_DIR/a100-d-nixl.pid"

wait_http http://127.0.0.1:31001/model_info 600
wait_http http://127.0.0.1:31002/model_info 600

"$SGLANG_PD_REPO/scripts/pd_snapshot_repro/run_gateway.sh" \
  >"$LOG_DIR/a100-pd-router.log" 2>&1 &
router_pid=$!
echo "$router_pid" >"$PID_DIR/a100-pd-router.pid"
wait_http http://127.0.0.1:31003/health 120

response="$LOG_DIR/a100-pd-nixl-greedy-logprob-response.json"
curl -fsS http://127.0.0.1:31003/generate \
  -H 'Content-Type: application/json' \
  -d '{"text":"The capital of France is","sampling_params":{"temperature":0,"max_new_tokens":12},"return_logprob":true,"logprob_start_len":0}' \
  >"$response"

python - "$response" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    result = json.load(f)
meta = result["meta_info"]
assert result["output_ids"], result
assert len(meta["output_token_logprobs"]) == len(result["output_ids"]), result
assert meta["weight_version"] == "baseline", result
print(json.dumps(result, ensure_ascii=False))
PY

grep -q 'CommonKVBootstrapServer started successfully' "$LOG_DIR/a100-p-nixl.log"
grep -q 'NIXL KVManager initialized with backend: UCX' "$LOG_DIR/a100-p-nixl.log"
grep -q 'NIXL KVManager initialized with backend: UCX' "$LOG_DIR/a100-d-nixl.log"
grep -q 'Prefill batch' "$LOG_DIR/a100-p-nixl.log"
echo "A100 NIXL PD smoke passed; logs: $LOG_DIR"
