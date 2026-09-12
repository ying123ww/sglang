#!/usr/bin/env bash
# CPU-only Phase I-a acceptance gates. No model processes are launched.
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
rust_env=${PD_RUST_ENV:-/public/home/yingxy2024/.conda/envs/slime}
export PATH="$rust_env/bin:$PATH"
export CARGO_HOME=${CARGO_HOME:-/public/home/yingxy2024/.tmp-codex/pd-version-cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/public/home/yingxy2024/.tmp-codex/pd-version-target}
export OPENSSL_DIR=${OPENSSL_DIR:-$rust_env}
export LD_LIBRARY_PATH="$OPENSSL_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
cd "$repo_dir"

mkdir -p "$repo_dir/.local/logs"
log_dir=$(mktemp -d "$repo_dir/.local/logs/phase-ia-XXXXXX")
exec > >(tee "$log_dir/acceptance.log") 2>&1
printf 'Evidence directory: %s\n' "$log_dir"
git rev-parse HEAD
git status --short
sha256sum sgl-model-gateway/Cargo.lock
cargo --version

cargo fmt --manifest-path sgl-model-gateway/Cargo.toml --check
for filter in \
  test_pd_weight_version \
  routers::http::pd_router::tests \
  policies::consistent_hashing::tests \
  test_filtered_candidate_set_falls_back_to_available_worker; do
  cargo test --locked -j 4 --manifest-path sgl-model-gateway/Cargo.toml \
    --lib "$filter" -- --nocapture
done
cargo check --locked -j 4 --manifest-path sgl-model-gateway/Cargo.toml --bins
for script in scripts/pd_snapshot_repro/*.sh; do
  bash -n "$script"
done
git diff --check
printf 'Phase I-a CPU acceptance passed. Log: %s/acceptance.log\n' "$log_dir"
