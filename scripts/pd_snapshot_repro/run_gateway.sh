#!/usr/bin/env bash
set -euo pipefail
source /public/home/yingxy2024/sglang-snapshot-serving/scripts/pd_snapshot_repro/common.sh

PORT=${PORT:-31003}
PREFILL_URL=${PREFILL_URL:-http://127.0.0.1:31001}
PREFILL_BOOTSTRAP_PORT=${PREFILL_BOOTSTRAP_PORT:-39001}
DECODE_URL=${DECODE_URL:-http://127.0.0.1:31002}
require_port_free "$PORT"

exec python -u -m sglang_router.launch_router --pd-disaggregation \
  --prefill "$PREFILL_URL" "$PREFILL_BOOTSTRAP_PORT" \
  --decode "$DECODE_URL" --host 127.0.0.1 --port "$PORT"
