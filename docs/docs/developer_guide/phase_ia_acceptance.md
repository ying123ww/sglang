# Phase I-a acceptance: compatible P/D selection

Status: **accepted for the deployment scope below**. Selection acceptance is
complete; this does not mark Phase I-b or the full Phase I project complete.

This milestone covers the charter's section 6 and SEL-01–SEL-13 plus META-01
and META-02. It applies to selection from trusted registration metadata in a
controlled deployment. Worker execution locks, metadata update ordering
(META-03–META-05), and KV transfer identity belong to Phase I-b.

## Final mode contract

- `legacy` (default): original independent P/D routing, including v10/v11
  and missing labels; retry version restrictions are disabled.
- `strict`: equal explicit non-blank, non-`default` versions; equal known
  canonical model IDs; equal `model_path` and `deployment_scope` whenever
  either worker supplies them. One-sided or blank identity fields reject.
- With no request model, multiple compatible model/source/deployment identities
  reject. With an explicit model, HTTP honors it even when IGW is disabled.
- When both workers omit scope, their registry is the single trusted deployment
  scope. A registry shared across deployments requires explicit scope labels.
- Version strings must be immutable identifiers assigned by the trusted
  deployment. I-a checks observed identity; it does not calculate weight hashes
  or promise safety against stale registrations during weight updates.

The legacy behavior intentionally differs from the first iteration of this
branch, which used legacy to mean equality including two missing versions.
That iteration did not implement charter SEL-13. Use strict to enable the fix.

## Routing semantics

Each policy receives the already compatible candidate slice. HTTP retains
separate P and D policies, invoked once each; gRPC retains its original model
policy. Returned indices address exactly the slices passed to the policy.

For consistent hashing, `x-smg-target-worker` indexes each role's **filtered
policy input**, not the original registry or a stable worker ID. The same
header is presented to P and D policies. Invalid, negative, or out-of-range
indices fail selection; they do not fall back to a different version. For a
stable affinity key use `x-smg-routing-key`; excluded ring URLs cannot reenter
the candidate slice. These header semantics apply to policies that support
them, as in the original Gateway.

HTTP selection failures use HTTP 503 / `server_selection_failed`; gRPC's
selection stage uses HTTP 503 / `no_available_pd_worker_pairs`. They occur
before dispatch and never trigger an automatic strict-to-legacy downgrade.

## Acceptance matrix

Test names below omit the common `test_pd_weight_version_` prefix. The shared
matrix runs the same scenarios through both production selectors.

| Charter case | Regression evidence |
| --- | --- |
| SEL-01 matching explicit versions | `matching_pair_is_selected`, `http_matching_pair_is_selected` |
| SEL-02 only mismatched versions | `mismatched_pair_is_rejected`, `http_mismatch_is_rejected` |
| SEL-03 valid pair inside mixed pool | `matching_pair_is_found_in_mixed_pool`, `http_finds_match_in_mixed_pool` |
| SEL-04 request bound to unavailable version | `retry_cannot_drift_to_new_version`, `http_retry_cannot_drift_to_new_version` |
| SEL-05 missing/unknown identity | `unknown_versions_rejected_by_both_selectors`, `model_source_and_scope_must_match` |
| SEL-06 unbound default placeholder | `strict_rejects_default_placeholder`, `http_strict_rejects_default`, `mode_contract` |
| SEL-07 same version, different model/source/deployment | `model_source_and_scope_must_match`, `ambiguous_deployment_pool_is_rejected` |
| SEL-08 independent HTTP policies | `http_independent_policies_and_filtered_indices` checks separate policy instances and one call each |
| SEL-09 non-IGW ambiguous identity / legacy unchanged | `ambiguous_model_pool_requires_request_identity`, `legacy_preserves_mismatch_and_retry_behavior` |
| SEL-10 filtered slice indices | `http_independent_policies_and_filtered_indices`, `hash_and_target_stay_inside_filtered_candidates` |
| SEL-11 ring initially selects excluded URL | `hash_and_target_stay_inside_filtered_candidates` constructs an actual key hitting that URL |
| SEL-12 explicit target conflict | same test checks valid filtered index plus out-of-range/negative/malformed targets |
| SEL-13 legacy original negative example | `legacy_preserves_mismatch_and_retry_behavior` returns v10/v11 deliberately |
| META-01 real HTTP discovery chain | `http_endpoint_discovery_registration_selection` |
| META-02 gRPC generated response → labels → selection | `from_grpc_model_info_reaches_selection`, `is_exposed_as_sglang_model_info_label` |
| Existing eligibility | `preserves_health_circuit_role_and_connection_filters` |
| Old configuration compatibility | `configuration_defaults_to_legacy` also rejects unknown mode strings |

META-01 uses ephemeral loopback HTTP servers and the production
`DiscoverMetadataStep → CreateLocalWorkerStep → RegisterWorkersStep →
UpdatePoliciesStep → ActivateWorkersStep → PDRouter` chain. It tests matching,
mismatched, absent, null, empty, and default versions, both through `/model_info`
and the 404 fallback to `/get_model_info`. No manual version label is inserted
between discovery and selection. META-02 uses the generated response type from
the locked `smg-grpc-client 1.0.0`; it is not a live gRPC server test.

## Reproduce

Final CPU gate result: exit code 0. Evidence:

```text
.local/logs/phase-ia-OYkc7J/acceptance.log
```

| Gate | Result |
| --- | --- |
| Version/identity/metadata contract suite | 27 passed |
| HTTP router regression suite | 14 passed (includes 5 tests also in the contract suite) |
| Existing consistent-hash regressions | 13 passed |
| Bucket filtered-candidate regression | 1 passed |
| Gateway binaries, `cargo check --locked --bins` | passed |
| Rust formatting, shell syntax, `git diff --check` | passed |

No GPU was used for this acceptance. The log's dirty entries contain only the
runner and documentation being finalized; all tested Rust source was committed.

Code under acceptance: `fa7f0baafa6d255743d13bf24f72a78e5f39b856`.
Cargo: `1.97.1`; locked dependency digest:

```text
4b3a76f19e0b3db91b1c218427e4bc7b62f06201f2300121a96362361c671938  sgl-model-gateway/Cargo.lock
```

```bash
bash scripts/pd_snapshot_repro/check_phase_ia.sh
```

The runner records a fresh acceptance log with source commit, worktree state,
Cargo.lock SHA256, focused contracts, existing HTTP router and consistent-hash
regressions, the Bucket regression, binary compilation, formatting, and shell
syntax. It does not launch GPU inference. GPU single-model/NIXL-PD evidence
and the remaining I-b protocol boundaries are recorded separately in
`pd_weight_version_selection_bugfix.md`.

## CPU cache-aware follow-up (2026-09-14)

Added `test_pd_weight_version_cache_aware_warm_prefix_contract` in
`sgl-model-gateway/src/routers/pd_selection_tests.rs`. No production code changed.
It calls the real CacheAwarePolicy, initializes its worker trees, warms a full
prompt on one P, and asserts that the unrestricted policy selects that P even
when a cold P is first in the candidate slice. The same prompt then enters the
production HTTP and gRPC P/D selectors. All workers have balanced zero load.

| Cached P | Other P | D | Mode | Actual result, both selectors |
| --- | --- | --- | --- | --- |
| v10 | cold v11 | v11 | legacy | cached v10 P + v11 D accepted |
| v10 | cold v11 | v11 | strict | cold v11 P + v11 D selected |
| v10 | absent | v11 | strict | selection rejected |
| v11 | cold v11 | v11 | strict | cached v11 P + v11 D selected |

Configuration explicitly matches Python `RouterArgs` defaults: threshold 0.3,
load thresholds 64/1.5, eviction 60 seconds, tree size 67108864. The Rust CLI
uses the same selection thresholds but an eviction interval of 120 seconds;
this short test does not exercise periodic eviction. It does not rely on
`RouterConfig::default()` or `CacheAwareConfig::default()`.

Result: **28 passed, 0 failed**, including the new eight-case matrix (four
scenarios times two transports). Log: `.local/logs/cpu-cache-aware-contract.log`.
Rerun with the Cargo environment documented by the CPU gate runner:

```bash
cargo test --locked -j4 --manifest-path sgl-model-gateway/Cargo.toml \
  --lib test_pd_weight_version -- --nocapture
```

This proves cache affinity alone does not enforce version compatibility, while
strict filtering takes precedence over affinity in these cases. Versions here
are controlled registry labels; there is no GPU allocation, real KV cache-hit
measurement, KV transfer, or inference with different weight snapshots. Cache
retention across version changes and performance are not validated by this test.
