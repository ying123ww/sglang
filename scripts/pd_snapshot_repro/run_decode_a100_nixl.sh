#!/usr/bin/env bash
set -euo pipefail
source /public/home/yingxy2024/sglang-snapshot-serving/scripts/pd_snapshot_repro/common.sh

GPU=${GPU:-7}
PORT=${PORT:-31002}
MODEL=${MODEL:-/storage/data/yingxy2024/Models/Qwen3-4B}
WEIGHT_VERSION=${WEIGHT_VERSION:-baseline}
IB_DEVICE=${IB_DEVICE:-mlx5_0}
require_gpu_free "$GPU"
require_port_free "$PORT"

exec env CUDA_VISIBLE_DEVICES="$GPU" python -u -m sglang.launch_server \
  --model-path "$MODEL" --host 127.0.0.1 --port "$PORT" --tp-size 1 \
  --mem-fraction-static 0.65 --disaggregation-mode decode \
  --disaggregation-transfer-backend nixl \
  --disaggregation-ib-device "$IB_DEVICE" --weight-version "$WEIGHT_VERSION"
