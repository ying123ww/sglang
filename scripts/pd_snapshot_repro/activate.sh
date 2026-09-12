#!/usr/bin/env bash

# Source this file. It selects a separate JIT cache for each GPU architecture.
SGLANG_PD_ENV=${SGLANG_PD_ENV:-/storage/data/yingxy2024/envs/sglang-pd-repro}
SGLANG_PD_REPO=${SGLANG_PD_REPO:-/public/home/yingxy2024/sglang-snapshot-serving}

case "$(hostname -s)" in
  ai_hgx_03) SGLANG_PD_ARCH=a100-sm80 ;;
  ai_gpu16) SGLANG_PD_ARCH=l20-sm89 ;;
  *) SGLANG_PD_ARCH="$(hostname -s)" ;;
esac

SGLANG_PD_CACHE=${SGLANG_PD_CACHE:-/storage/data/yingxy2024/sglang-pd-repro-cache/$SGLANG_PD_ARCH}
mkdir -p \
  "$SGLANG_PD_CACHE/cuda" \
  "$SGLANG_PD_CACHE/flashinfer" \
  "$SGLANG_PD_CACHE/torch-extensions" \
  "$SGLANG_PD_CACHE/triton" \
  "$SGLANG_PD_CACHE/tvm-ffi" \
  "$SGLANG_PD_CACHE/tmp"

export PATH="$SGLANG_PD_ENV/bin:$PATH"
export PYTHONPATH="$SGLANG_PD_REPO/python"
export PYTHONNOUSERSITE=1
export CUDA_HOME="$SGLANG_PD_ENV"
export CUDACXX="$SGLANG_PD_ENV/bin/nvcc"
export CPATH="$SGLANG_PD_ENV/include${CPATH:+:$CPATH}"
export LIBRARY_PATH="$SGLANG_PD_ENV/lib:$SGLANG_PD_ENV/targets/x86_64-linux/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
export LD_LIBRARY_PATH="$SGLANG_PD_ENV/lib/python3.12/site-packages/torch/lib:$SGLANG_PD_ENV/lib/python3.12/site-packages/nvidia/cuda_runtime/lib:$SGLANG_PD_ENV/lib/python3.12/site-packages/nvidia/cudnn/lib:$SGLANG_PD_ENV/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export TMPDIR="$SGLANG_PD_CACHE/tmp"
export SGLANG_NUMA_BIND_V2=0
export CUDA_CACHE_PATH="$SGLANG_PD_CACHE/cuda"
export FLASHINFER_WORKSPACE_BASE="$SGLANG_PD_CACHE/flashinfer"
export TORCH_EXTENSIONS_DIR="$SGLANG_PD_CACHE/torch-extensions"
export TRITON_CACHE_DIR="$SGLANG_PD_CACHE/triton"
export TVM_FFI_CACHE_DIR="$SGLANG_PD_CACHE/tvm-ffi"

unset SGLANG_PD_ARCH
