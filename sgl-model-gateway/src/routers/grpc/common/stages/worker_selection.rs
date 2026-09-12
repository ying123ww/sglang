//! Worker selection stage: Select appropriate worker(s) based on routing mode

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::response::Response;
use tracing::{error, warn};

use super::PipelineStage;
use crate::{
    config::PdWeightVersionMode,
    core::{ConnectionMode, Worker, WorkerRegistry, WorkerType, UNKNOWN_MODEL_ID},
    observability::metrics::{metrics_labels, Metrics},
    policies::{PolicyRegistry, SelectWorkerInfo},
    routers::{
        error,
        grpc::context::{RequestContext, WorkerSelection},
    },
};

/// Worker selection stage: Select appropriate worker(s) based on routing mode
pub(crate) struct WorkerSelectionStage {
    worker_registry: Arc<WorkerRegistry>,
    policy_registry: Arc<PolicyRegistry>,
    mode: WorkerSelectionMode,
    pd_weight_version_mode: PdWeightVersionMode,
}

pub(crate) enum WorkerSelectionMode {
    /// Regular mode: select single worker
    Regular,
    /// PD mode: select prefill + decode workers
    PrefillDecode,
}

fn weight_version(worker: &Arc<dyn Worker>) -> Option<&str> {
    worker
        .metadata()
        .labels
        .get("weight_version")
        .map(String::as_str)
}

fn pd_model_identity(worker: &Arc<dyn Worker>) -> Option<(&str, Option<&str>, Option<&str>)> {
    let model = worker.model_id();
    if model.trim().is_empty() || model == UNKNOWN_MODEL_ID || model == "default" {
        return None;
    }
    let labels = &worker.metadata().labels;
    let path = labels.get("model_path").map(String::as_str);
    let scope = labels.get("deployment_scope").map(String::as_str);
    if path.is_some_and(|s| s.trim().is_empty()) || scope.is_some_and(|s| s.trim().is_empty()) {
        return None;
    }
    Some((model, path, scope))
}

impl WorkerSelectionStage {
    pub fn new(
        worker_registry: Arc<WorkerRegistry>,
        policy_registry: Arc<PolicyRegistry>,
        mode: WorkerSelectionMode,
    ) -> Self {
        Self {
            worker_registry,
            policy_registry,
            mode,
            pd_weight_version_mode: PdWeightVersionMode::Legacy,
        }
    }

    pub fn with_pd_weight_version_mode(mut self, mode: PdWeightVersionMode) -> Self {
        self.pd_weight_version_mode = mode;
        self
    }
}

#[async_trait]
impl PipelineStage for WorkerSelectionStage {
    async fn execute(&self, ctx: &mut RequestContext) -> Result<Option<Response>, Response> {
        let prep = ctx.state.preparation.as_ref().ok_or_else(|| {
            error!(
                function = "WorkerSelectionStage::execute",
                "Preparation stage not completed"
            );
            error::internal_error(
                "preparation_stage_not_completed",
                "Preparation stage not completed",
            )
        })?;

        // For Harmony, use selection_text produced during Harmony encoding
        // Otherwise, use original_text from regular preparation
        let text = if prep.harmony_mode {
            prep.selection_text.as_deref()
        } else {
            prep.original_text.as_deref()
        };

        // Get tokens for PrefixHash policy support
        let tokens = if prep.token_ids.is_empty() {
            None
        } else {
            Some(prep.token_ids.as_slice())
        };

        let headers = ctx.input.headers.as_ref();

        let workers = match self.mode {
            WorkerSelectionMode::Regular => {
                match self
                    .select_single_worker(ctx.input.model_id.as_deref(), text, tokens, headers)
                    .await
                {
                    Some(w) => WorkerSelection::Single { worker: w },
                    None => {
                        let model = ctx.input.model_id.as_deref().unwrap_or(UNKNOWN_MODEL_ID);
                        error!(
                            function = "WorkerSelectionStage::execute",
                            mode = "Regular",
                            model_id = %model,
                            "No available workers for model"
                        );
                        return Err(error::service_unavailable(
                            "no_available_workers",
                            format!("No available workers for model: {}", model),
                        ));
                    }
                }
            }
            WorkerSelectionMode::PrefillDecode => {
                match self
                    .select_pd_pair_with_bound(
                        ctx.input.model_id.as_deref(),
                        text,
                        tokens,
                        headers,
                        ctx.state.bound_pd_weight_version.as_deref(),
                    )
                    .await
                {
                    Some((prefill, decode)) => WorkerSelection::Dual { prefill, decode },
                    None => {
                        let model = ctx.input.model_id.as_deref().unwrap_or(UNKNOWN_MODEL_ID);
                        error!(
                            function = "WorkerSelectionStage::execute",
                            mode = "PrefillDecode",
                            model_id = %model,
                            "No available PD worker pairs for model"
                        );
                        return Err(error::service_unavailable(
                            "no_available_pd_worker_pairs",
                            format!("No available PD worker pairs for model: {}", model),
                        ));
                    }
                }
            }
        };

        ctx.state.workers = Some(workers);
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "WorkerSelection"
    }
}

impl WorkerSelectionStage {
    async fn select_single_worker(
        &self,
        model_id: Option<&str>,
        text: Option<&str>,
        tokens: Option<&[u32]>,
        headers: Option<&http::HeaderMap>,
    ) -> Option<Arc<dyn Worker>> {
        // Get workers for the specified model, filtered by connection mode
        let workers = self.worker_registry.get_workers_filtered(
            model_id,
            Some(WorkerType::Regular),
            Some(ConnectionMode::Grpc { port: None }),
            None,  // any runtime type
            false, // get all workers, we'll filter by is_available() next
        );

        // Use into_iter() to take ownership of Arcs without cloning (avoids atomic inc/dec)
        let available: Vec<Arc<dyn Worker>> =
            workers.into_iter().filter(|w| w.is_available()).collect();

        if available.is_empty() {
            return None;
        }

        // Get the appropriate policy for this model
        let policy = match model_id {
            Some(model) => self.policy_registry.get_policy_or_default(model),
            None => self.policy_registry.get_default_policy(),
        };

        // Get cached hash ring for consistent hashing (O(log n) lookup)
        let hash_ring = self
            .worker_registry
            .get_hash_ring(model_id.unwrap_or(UNKNOWN_MODEL_ID));

        // Select worker using the policy
        let idx = policy
            .select_worker(
                &available,
                &SelectWorkerInfo {
                    request_text: text,
                    tokens,
                    headers,
                    hash_ring,
                },
            )
            .await?;
        let selected = available[idx].clone();

        // Record worker selection metric
        Metrics::record_worker_selection(
            metrics_labels::WORKER_REGULAR,
            metrics_labels::CONNECTION_GRPC,
            model_id.unwrap_or(UNKNOWN_MODEL_ID),
            policy.name(),
        );

        Some(selected)
    }

    #[cfg(test)]
    async fn select_pd_pair(
        &self,
        model_id: Option<&str>,
        text: Option<&str>,
        tokens: Option<&[u32]>,
        headers: Option<&http::HeaderMap>,
    ) -> Option<(Arc<dyn Worker>, Arc<dyn Worker>)> {
        self.select_pd_pair_with_bound(model_id, text, tokens, headers, None)
            .await
    }

    pub(crate) async fn select_pd_pair_with_bound(
        &self,
        model_id: Option<&str>,
        text: Option<&str>,
        tokens: Option<&[u32]>,
        headers: Option<&http::HeaderMap>,
        bound_weight_version: Option<&OnceLock<Option<String>>>,
    ) -> Option<(Arc<dyn Worker>, Arc<dyn Worker>)> {
        let strict = self.pd_weight_version_mode == PdWeightVersionMode::Strict;
        // Legacy retains independent selection on every attempt, including retries.
        let bound_weight_version = bound_weight_version.filter(|_| strict);
        let all_workers = self.worker_registry.get_workers_filtered(
            model_id,
            None,
            Some(ConnectionMode::Grpc { port: None }), // Match any gRPC worker
            None,                                      // any runtime type
            false,
        );

        let (available_prefill, available_decode): (Vec<_>, Vec<_>) =
            all_workers
                .into_iter()
                .fold((Vec::new(), Vec::new()), |mut acc, w| {
                    if w.is_available() {
                        match w.metadata().worker_type {
                            WorkerType::Prefill { .. } => acc.0.push(w),
                            WorkerType::Decode => acc.1.push(w),
                            _ => {}
                        }
                    }
                    acc
                });

        let matches_bound_version =
            |worker: &Arc<dyn Worker>| match bound_weight_version.and_then(OnceLock::get) {
                Some(bound) => weight_version(worker) == bound.as_deref(),
                None => true,
            };
        let available_prefill: Vec<_> = available_prefill
            .into_iter()
            .filter(&matches_bound_version)
            .collect();
        let available_decode: Vec<_> = available_decode
            .into_iter()
            .filter(&matches_bound_version)
            .collect();

        if available_prefill.is_empty() {
            warn!("No available prefill workers");
            return None;
        }

        if available_decode.is_empty() {
            warn!("No available decode workers");
            return None;
        }

        let compatible = |prefill: &Arc<dyn Worker>, decode: &Arc<dyn Worker>| {
            if !strict {
                return true;
            }
            let identity = pd_model_identity(prefill);
            identity.is_some()
                && identity == pd_model_identity(decode)
                && self
                    .pd_weight_version_mode
                    .compatible(weight_version(prefill), weight_version(decode))
        };
        // Apply strict identity constraints before invoking either policy.
        let compatible_prefill: Vec<_> = available_prefill
            .into_iter()
            .filter(|prefill| {
                available_decode
                    .iter()
                    .any(|decode| compatible(prefill, decode))
            })
            .collect();

        if compatible_prefill.is_empty() {
            warn!("No weight-version-compatible PD worker pairs");
            return None;
        }

        // Without a requested model, strict mode needs one unambiguous model pool.
        if strict && model_id.is_none() {
            let first_model = pd_model_identity(&compatible_prefill[0]);
            if compatible_prefill
                .iter()
                .any(|w| pd_model_identity(w) != first_model)
            {
                warn!("Strict PD selection requires an unambiguous model identity");
                return None;
            }
        }

        // Select using policies
        let policy = match model_id {
            Some(model) => self.policy_registry.get_policy_or_default(model),
            None => self.policy_registry.get_default_policy(),
        };

        // Get cached hash ring for consistent hashing (O(log n) lookup)
        let hash_ring = self
            .worker_registry
            .get_hash_ring(model_id.unwrap_or(UNKNOWN_MODEL_ID));

        let info = SelectWorkerInfo {
            request_text: text,
            tokens,
            headers,
            hash_ring,
        };
        let prefill_idx = policy.select_worker(&compatible_prefill, &info).await?;
        let selected_prefill = compatible_prefill.get(prefill_idx)?.clone();
        let compatible_decode: Vec<_> = available_decode
            .into_iter()
            .filter(|decode| compatible(&selected_prefill, decode))
            .collect();
        let decode_idx = policy.select_worker(&compatible_decode, &info).await?;

        let model = model_id.unwrap_or(UNKNOWN_MODEL_ID);
        let policy_name = policy.name();

        // Record worker selection metrics for both prefill and decode
        Metrics::record_worker_selection(
            metrics_labels::WORKER_PREFILL,
            metrics_labels::CONNECTION_GRPC,
            model,
            policy_name,
        );
        Metrics::record_worker_selection(
            metrics_labels::WORKER_DECODE,
            metrics_labels::CONNECTION_GRPC,
            model,
            policy_name,
        );

        let selected_decode = compatible_decode.get(decode_idx)?.clone();
        if let Some(bound) = bound_weight_version {
            let selected_version = weight_version(&selected_prefill).map(str::to_string);
            if bound.set(selected_version).is_err() {
                debug_assert_eq!(
                    bound.get().and_then(Option::as_deref),
                    weight_version(&selected_prefill)
                );
            }
        }

        Some((selected_prefill, selected_decode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PolicyConfig, core::BasicWorkerBuilder, routers::grpc::client::ModelInfo};

    fn stage_with_pd_version_sets(
        prefill_versions: &[&str],
        decode_versions: &[&str],
    ) -> WorkerSelectionStage {
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::RoundRobin));

        for (index, version) in prefill_versions.iter().enumerate() {
            let worker = Arc::new(
                BasicWorkerBuilder::new(format!("http://prefill:{}", 30000 + index * 2))
                    .worker_type(WorkerType::Prefill {
                        bootstrap_port: Some(8998),
                    })
                    .connection_mode(ConnectionMode::Grpc { port: None })
                    .label("model_id", "qwen")
                    .label("weight_version", *version)
                    .build(),
            );
            assert!(worker.is_available());
            worker_registry.register(worker);
            policy_registry.on_worker_added("qwen", None);
        }

        for (index, version) in decode_versions.iter().enumerate() {
            let worker = Arc::new(
                BasicWorkerBuilder::new(format!("http://decode:{}", 30001 + index * 2))
                    .worker_type(WorkerType::Decode)
                    .connection_mode(ConnectionMode::Grpc { port: None })
                    .label("model_id", "qwen")
                    .label("weight_version", *version)
                    .build(),
            );
            assert!(worker.is_available());
            worker_registry.register(worker);
            policy_registry.on_worker_added("qwen", None);
        }
        assert_eq!(
            worker_registry.get_by_model("qwen").len(),
            prefill_versions.len() + decode_versions.len()
        );

        WorkerSelectionStage::new(
            worker_registry,
            policy_registry,
            WorkerSelectionMode::PrefillDecode,
        )
        .with_pd_weight_version_mode(PdWeightVersionMode::Strict)
    }

    fn stage_with_pd_versions(prefill_version: &str, decode_version: &str) -> WorkerSelectionStage {
        stage_with_pd_version_sets(&[prefill_version], &[decode_version])
    }

    fn stage_with_unlabeled_pd_workers() -> WorkerSelectionStage {
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::RoundRobin));
        let prefill = Arc::new(
            BasicWorkerBuilder::new("http://prefill:30000")
                .worker_type(WorkerType::Prefill {
                    bootstrap_port: Some(8998),
                })
                .connection_mode(ConnectionMode::Grpc { port: None })
                .build(),
        );
        let decode = Arc::new(
            BasicWorkerBuilder::new("http://decode:30001")
                .worker_type(WorkerType::Decode)
                .connection_mode(ConnectionMode::Grpc { port: None })
                .build(),
        );
        worker_registry.register(prefill);
        worker_registry.register(decode);

        WorkerSelectionStage::new(
            worker_registry,
            policy_registry,
            WorkerSelectionMode::PrefillDecode,
        )
    }

    #[tokio::test]
    async fn test_pd_weight_version_matching_pair_is_selected() {
        let stage = stage_with_pd_versions("v11", "v11");
        let (prefill, decode) = stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .expect("a healthy same-version PD pair should be selectable");

        assert_eq!(prefill.url(), "http://prefill:30000");
        assert_eq!(decode.url(), "http://decode:30001");
        assert_eq!(prefill.metadata().labels["weight_version"], "v11");
        assert_eq!(decode.metadata().labels["weight_version"], "v11");
    }

    // This checks admission only; it does not exercise KV transfer or decode.
    #[tokio::test]
    async fn test_pd_weight_version_mismatched_pair_is_rejected() {
        let stage = stage_with_pd_versions("v10", "v11");
        let selected_versions = stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .map(|(prefill, decode)| {
                (
                    prefill.metadata().labels["weight_version"].clone(),
                    decode.metadata().labels["weight_version"].clone(),
                )
            });

        assert_eq!(
            selected_versions, None,
            "no compatible PD pair exists: prefill v10 must not be paired with decode v11"
        );
    }

    #[tokio::test]
    async fn test_pd_weight_version_matching_pair_is_found_in_mixed_pool() {
        let stage = stage_with_pd_version_sets(&["v10", "v11"], &["v11"]);
        let (prefill, decode) = stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .expect("the compatible v11 pair must not be hidden by the v10 prefill");

        assert_eq!(prefill.metadata().labels["weight_version"], "v11");
        assert_eq!(decode.metadata().labels["weight_version"], "v11");
    }

    #[tokio::test]
    async fn test_pd_weight_version_from_grpc_model_info_reaches_selection() {
        let labels_from_response = |version: &str| {
            ModelInfo::Sglang(Box::new(
                smg_grpc_client::sglang_proto::GetModelInfoResponse {
                    model_path: "/models/qwen".to_string(),
                    weight_version: version.to_string(),
                    ..Default::default()
                },
            ))
            .to_labels()
        };
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::RoundRobin));

        let prefill = Arc::new(
            BasicWorkerBuilder::new("http://prefill:30000")
                .worker_type(WorkerType::Prefill {
                    bootstrap_port: Some(8998),
                })
                .connection_mode(ConnectionMode::Grpc { port: None })
                .labels(labels_from_response("v11"))
                .label("model_id", "qwen")
                .build(),
        );
        let decode = Arc::new(
            BasicWorkerBuilder::new("http://decode:30001")
                .worker_type(WorkerType::Decode)
                .connection_mode(ConnectionMode::Grpc { port: None })
                .labels(labels_from_response("v11"))
                .label("model_id", "qwen")
                .build(),
        );
        worker_registry.register(prefill);
        worker_registry.register(decode);
        policy_registry.on_worker_added("qwen", None);
        policy_registry.on_worker_added("qwen", None);

        let stage = WorkerSelectionStage::new(
            worker_registry,
            policy_registry,
            WorkerSelectionMode::PrefillDecode,
        )
        .with_pd_weight_version_mode(PdWeightVersionMode::Strict);
        let (prefill, decode) = stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .expect("versions discovered from real gRPC response fields must be selectable");

        assert_eq!(prefill.metadata().labels["weight_version"], "v11");
        assert_eq!(decode.metadata().labels["weight_version"], "v11");
    }

    #[tokio::test]
    async fn test_pd_weight_version_legacy_accepts_two_unlabeled_workers() {
        assert!(stage_with_unlabeled_pd_workers()
            .select_pd_pair(None, None, None, None)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_pd_weight_version_strict_accepts_explicit_matching_version() {
        let stage = stage_with_pd_versions("v11", "v11")
            .with_pd_weight_version_mode(PdWeightVersionMode::Strict);
        assert!(stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_pd_weight_version_strict_rejects_unlabeled_workers() {
        let stage = stage_with_unlabeled_pd_workers()
            .with_pd_weight_version_mode(PdWeightVersionMode::Strict);
        assert!(stage.select_pd_pair(None, None, None, None).await.is_none());
    }

    #[tokio::test]
    async fn test_pd_weight_version_strict_rejects_default_placeholder() {
        let stage = stage_with_pd_versions("default", "default")
            .with_pd_weight_version_mode(PdWeightVersionMode::Strict);
        assert!(stage
            .select_pd_pair(Some("qwen"), None, None, None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_pd_weight_version_retry_cannot_drift_to_new_version() {
        let bound = OnceLock::new();
        let first_attempt = stage_with_pd_versions("v10", "v10");
        assert!(first_attempt
            .select_pd_pair_with_bound(Some("qwen"), None, None, None, Some(&bound))
            .await
            .is_some());
        assert_eq!(bound.get().and_then(Option::as_deref), Some("v10"));

        let new_version_only = stage_with_pd_versions("v11", "v11");
        assert!(new_version_only
            .select_pd_pair_with_bound(Some("qwen"), None, None, None, Some(&bound))
            .await
            .is_none());

        let replacement_same_version = stage_with_pd_versions("v10", "v10");
        assert!(replacement_same_version
            .select_pd_pair_with_bound(Some("qwen"), None, None, None, Some(&bound))
            .await
            .is_some());
    }
}
