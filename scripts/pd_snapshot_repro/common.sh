#!/usr/bin/env bash
set -euo pipefail

SGLANG_PD_REPO=${SGLANG_PD_REPO:-/public/home/yingxy2024/sglang-snapshot-serving}
source "$SGLANG_PD_REPO/scripts/pd_snapshot_repro/activate.sh"

if [[ "$(hostname -s)" != ai_hgx_03 ]]; then
  echo "These A100 scripts must run on ai_hgx_03" >&2
  exit 2
fi

require_port_free() {
  local port=$1
  if ss -ltn | awk '{print $4}' | grep -Eq ":${port}$"; then
    echo "Port ${port} is already listening" >&2
    exit 2
  fi
}

require_gpu_free() {
  local gpu=$1
  if nvidia-smi -i "$gpu" --query-compute-apps=pid --format=csv,noheader | grep -Eq '[0-9]'; then
    echo "GPU ${gpu} already has a compute process" >&2
    exit 2
  fi
}

wait_http() {
  local url=$1
  local timeout=${2:-300}
  local start=$SECONDS
  until curl -fsS "$url" >/dev/null; do
    if (( SECONDS - start >= timeout )); then
      echo "Timed out waiting for ${url}" >&2
      return 1
    fi
    sleep 2
  done
}

cd "$TMPDIR"
