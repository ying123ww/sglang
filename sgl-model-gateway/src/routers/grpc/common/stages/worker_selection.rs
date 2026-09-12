//! Worker selection stage: Select appropriate worker(s) based on routing mode

use std::sync::Arc;

use async_trait::async_trait;
use axum::response::Response;
use tracing::{error, warn};

use super::PipelineStage;
use crate::{
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
        }
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
                    .select_pd_pair(ctx.input.model_id.as_deref(), text, tokens, headers)
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

    async fn select_pd_pair(
        &self,
        model_id: Option<&str>,
        text: Option<&str>,
        tokens: Option<&[u32]>,
        headers: Option<&http::HeaderMap>,
    ) -> Option<(Arc<dyn Worker>, Arc<dyn Worker>)> {
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

        if available_prefill.is_empty() {
            warn!("No available prefill workers");
            return None;
        }

        if available_decode.is_empty() {
            warn!("No available decode workers");
            return None;
        }

        // Apply the hard compatibility constraint before invoking the policy. Two
        // unlabeled workers remain compatible for legacy deployments.
        let compatible_prefill: Vec<_> = available_prefill
            .into_iter()
            .filter(|prefill| {
                available_decode
                    .iter()
                    .any(|decode| weight_version(prefill) == weight_version(decode))
            })
            .collect();

        if compatible_prefill.is_empty() {
            warn!("No weight-version-compatible PD worker pairs");
            return None;
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
        let selected_prefill = compatible_prefill[prefill_idx].clone();
        let compatible_decode: Vec<_> = available_decode
            .into_iter()
            .filter(|decode| weight_version(&selected_prefill) == weight_version(decode))
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

        Some((selected_prefill, compatible_decode[decode_idx].clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PolicyConfig, core::BasicWorkerBuilder};

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
    }

    fn stage_with_pd_versions(prefill_version: &str, decode_version: &str) -> WorkerSelectionStage {
        stage_with_pd_version_sets(&[prefill_version], &[decode_version])
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

    // Intentionally red until selection enforces PD weight-version compatibility.
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
}
