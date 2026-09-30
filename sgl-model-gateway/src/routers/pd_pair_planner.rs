use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};

use crate::{
    config::types::PdWeightVersionMode,
    core::{ConnectionMode, Worker, UNKNOWN_MODEL_ID},
    policies::{LoadBalancingPolicy, SelectWorkerInfo},
};

/// Parameters for PD pair planning.
pub struct PdPlanParams<'a> {
    pub prefill_workers: Vec<Arc<dyn Worker>>,
    pub decode_workers: Vec<Arc<dyn Worker>>,
    pub prefill_policy: &'a dyn LoadBalancingPolicy,
    pub decode_policy: &'a dyn LoadBalancingPolicy,
    pub mode: PdWeightVersionMode,
    pub model_id: Option<&'a str>,
    pub bound_weight_version: Option<&'a OnceLock<Option<String>>>,
    pub select_info: &'a SelectWorkerInfo<'a>,
    /// Optional required connection mode (e.g., Http or Grpc).
    pub required_connection_mode: Option<ConnectionMode>,
}

/// Errors returned by the PD pair planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdPlanError {
    NoPrefillWorkers,
    NoDecodeWorkers,
    NoCompatiblePair(PdWeightVersionMode),
    AmbiguousModelIdentity,
    PolicyFailed {
        policy_name: String,
        role: &'static str,
    },
}

impl std::fmt::Display for PdPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPrefillWorkers => write!(
                f,
                "No prefill workers available. Please check if prefill servers are configured and healthy."
            ),
            Self::NoDecodeWorkers => write!(
                f,
                "No decode workers available. Please check if decode servers are configured and healthy."
            ),
            Self::NoCompatiblePair(mode) => write!(
                f,
                "No model-identity and weight-version compatible PD pair in {:?} mode",
                mode
            ),
            Self::AmbiguousModelIdentity => write!(
                f,
                "Ambiguous strict PD model identity; specify a model or use a single deployment pool"
            ),
            Self::PolicyFailed { policy_name, role } => write!(
                f,
                "Policy {} failed to select a {} worker",
                policy_name, role
            ),
        }
    }
}

impl std::error::Error for PdPlanError {}

/// Extract the weight_version string from worker labels if present.
pub fn weight_version(worker: &Arc<dyn Worker>) -> Option<&str> {
    worker
        .metadata()
        .labels
        .get("weight_version")
        .map(String::as_str)
}

/// Extract model identity tuple `(model_id, model_path, deployment_scope)` for strict PD matching.
pub fn pd_model_identity(worker: &Arc<dyn Worker>) -> Option<(&str, Option<&str>, Option<&str>)> {
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

/// Compatibility key for hashing and grouping workers in Strict PD mode:
/// `(model_id, model_path, deployment_scope, weight_version)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PdCompatibilityKey<'a> {
    pub model_id: &'a str,
    pub model_path: Option<&'a str>,
    pub deployment_scope: Option<&'a str>,
    pub weight_version: &'a str,
}

impl<'a> PdCompatibilityKey<'a> {
    /// Attempt to derive a compatibility key for a worker.
    /// Returns `None` if model identity or weight version is missing, empty, or placeholder.
    pub fn from_worker(worker: &'a Arc<dyn Worker>) -> Option<Self> {
        let (model, path, scope) = pd_model_identity(worker)?;
        let wv = weight_version(worker)?;
        if wv.trim().is_empty() || wv == "default" {
            return None;
        }
        Some(Self {
            model_id: model,
            model_path: path,
            deployment_scope: scope,
            weight_version: wv,
        })
    }

    /// Return the model identity portion of the key.
    pub fn model_identity(&self) -> (&'a str, Option<&'a str>, Option<&'a str>) {
        (self.model_id, self.model_path, self.deployment_scope)
    }
}

/// Check whether a prefill and decode worker form a compatible pair under the given mode.
pub fn compatible_pair(
    prefill: &Arc<dyn Worker>,
    decode: &Arc<dyn Worker>,
    mode: PdWeightVersionMode,
) -> bool {
    if mode == PdWeightVersionMode::Legacy {
        return true;
    }
    match (
        PdCompatibilityKey::from_worker(prefill),
        PdCompatibilityKey::from_worker(decode),
    ) {
        (Some(pk), Some(dk)) => pk == dk,
        _ => false,
    }
}

/// Shared semi-join PD pair planner using Hash Semi-Join algorithm (O(P + D)).
pub struct PdPairPlanner;

impl PdPairPlanner {
    /// Plan and select a compatible (Prefill, Decode) worker pair.
    pub async fn plan(
        params: PdPlanParams<'_>,
    ) -> Result<(Arc<dyn Worker>, Arc<dyn Worker>), PdPlanError> {
        let strict = params.mode == PdWeightVersionMode::Strict;
        let bound_weight_version = if strict {
            params.bound_weight_version
        } else {
            None
        };

        if params.prefill_workers.is_empty() {
            return Err(PdPlanError::NoPrefillWorkers);
        }
        if params.decode_workers.is_empty() {
            return Err(PdPlanError::NoDecodeWorkers);
        }

        let matches_bound_version =
            |worker: &Arc<dyn Worker>| match bound_weight_version.and_then(OnceLock::get) {
                Some(bound) => weight_version(worker) == bound.as_deref(),
                None => true,
            };

        let required_conn = params.required_connection_mode.as_ref();
        let matches_connection = |worker: &Arc<dyn Worker>| match required_conn {
            Some(mode) => !strict || &worker.metadata().connection_mode == mode,
            None => true,
        };

        let available_prefill: Vec<_> = params
            .prefill_workers
            .into_iter()
            .filter(|w| w.is_available() && matches_bound_version(w) && matches_connection(w))
            .collect();

        let available_decode: Vec<_> = params
            .decode_workers
            .into_iter()
            .filter(|w| w.is_available() && matches_bound_version(w) && matches_connection(w))
            .collect();

        if available_prefill.is_empty() && !available_decode.is_empty() {
            if strict {
                return Err(PdPlanError::NoCompatiblePair(params.mode));
            } else {
                return Err(PdPlanError::NoPrefillWorkers);
            }
        }
        if available_decode.is_empty() && !available_prefill.is_empty() {
            if strict {
                return Err(PdPlanError::NoCompatiblePair(params.mode));
            } else {
                return Err(PdPlanError::NoDecodeWorkers);
            }
        }
        if available_prefill.is_empty() && available_decode.is_empty() {
            return Err(PdPlanError::NoPrefillWorkers);
        }

        // Fast path for Legacy mode: no hashing or version constraints needed (O(1)).
        if !strict {
            let prefill_idx = params
                .prefill_policy
                .select_worker(&available_prefill, params.select_info)
                .await
                .ok_or_else(|| PdPlanError::PolicyFailed {
                    policy_name: params.prefill_policy.name().to_string(),
                    role: "prefill",
                })?;
            let selected_prefill =
                available_prefill.get(prefill_idx).cloned().ok_or_else(|| {
                    PdPlanError::PolicyFailed {
                        policy_name: params.prefill_policy.name().to_string(),
                        role: "prefill",
                    }
                })?;

            let decode_idx = params
                .decode_policy
                .select_worker(&available_decode, params.select_info)
                .await
                .ok_or_else(|| PdPlanError::PolicyFailed {
                    policy_name: params.decode_policy.name().to_string(),
                    role: "decode",
                })?;
            let selected_decode = available_decode.get(decode_idx).cloned().ok_or_else(|| {
                PdPlanError::PolicyFailed {
                    policy_name: params.decode_policy.name().to_string(),
                    role: "decode",
                }
            })?;

            return Ok((selected_prefill, selected_decode));
        }

        // Strict mode: Hash Semi-Join in O(P + D) time.
        // 1. Group decode candidates by PdCompatibilityKey: O(D)
        let mut decode_map: HashMap<PdCompatibilityKey<'_>, Vec<Arc<dyn Worker>>> = HashMap::new();
        for decode in &available_decode {
            if let Some(key) = PdCompatibilityKey::from_worker(decode) {
                decode_map.entry(key).or_default().push(Arc::clone(decode));
            }
        }

        if decode_map.is_empty() {
            return Err(PdPlanError::NoCompatiblePair(params.mode));
        }

        // 2. Hash Semi-Join: prune prefill candidates with O(1) hash map lookup: O(P)
        let compatible_prefill: Vec<_> = available_prefill
            .into_iter()
            .filter(|prefill| {
                PdCompatibilityKey::from_worker(prefill)
                    .map_or(false, |key| decode_map.contains_key(&key))
            })
            .collect();

        if compatible_prefill.is_empty() {
            return Err(PdPlanError::NoCompatiblePair(params.mode));
        }

        // 3. Ambiguous model identity check
        if params.model_id.is_none() {
            let identities: HashSet<_> = compatible_prefill
                .iter()
                .filter_map(pd_model_identity)
                .collect();
            if identities.len() > 1 {
                return Err(PdPlanError::AmbiguousModelIdentity);
            }
        }

        // 4. Select prefill worker using prefill policy
        let prefill_idx = params
            .prefill_policy
            .select_worker(&compatible_prefill, params.select_info)
            .await
            .ok_or_else(|| PdPlanError::PolicyFailed {
                policy_name: params.prefill_policy.name().to_string(),
                role: "prefill",
            })?;
        let selected_prefill = compatible_prefill
            .get(prefill_idx)
            .cloned()
            .ok_or_else(|| PdPlanError::PolicyFailed {
                policy_name: params.prefill_policy.name().to_string(),
                role: "prefill",
            })?;

        // 5. Retrieve compatible decode candidates in O(1) from decode_map
        let selected_key = PdCompatibilityKey::from_worker(&selected_prefill)
            .ok_or_else(|| PdPlanError::NoCompatiblePair(params.mode))?;

        let compatible_decode = decode_map.remove(&selected_key).unwrap_or_default();

        if compatible_decode.is_empty() {
            return Err(PdPlanError::NoCompatiblePair(params.mode));
        }

        // 6. Select decode worker using decode policy
        let decode_idx = params
            .decode_policy
            .select_worker(&compatible_decode, params.select_info)
            .await
            .ok_or_else(|| PdPlanError::PolicyFailed {
                policy_name: params.decode_policy.name().to_string(),
                role: "decode",
            })?;
        let selected_decode = compatible_decode.get(decode_idx).cloned().ok_or_else(|| {
            PdPlanError::PolicyFailed {
                policy_name: params.decode_policy.name().to_string(),
                role: "decode",
            }
        })?;

        // 7. Pin bound weight version if requested
        if let Some(bound) = bound_weight_version {
            let selected_version = Some(selected_key.weight_version.to_string());
            if bound.set(selected_version).is_err() {
                debug_assert_eq!(
                    bound.get().and_then(Option::as_deref),
                    Some(selected_key.weight_version)
                );
            }
        }

        Ok((selected_prefill, selected_decode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{BasicWorkerBuilder, WorkerType},
        policies::{RandomPolicy, SelectWorkerInfo},
    };

    fn make_test_worker(
        url: &str,
        role: WorkerType,
        model: &str,
        version: Option<&str>,
    ) -> Arc<dyn Worker> {
        let mut builder = BasicWorkerBuilder::new(url)
            .worker_type(role)
            .label("model_id", model);
        if let Some(v) = version {
            builder = builder.label("weight_version", v);
        }
        let w = Arc::new(builder.build());
        w.set_healthy(true);
        w
    }

    #[test]
    fn test_pd_compatibility_key_and_pair() {
        let p_v10 = make_test_worker(
            "http://p1",
            WorkerType::Prefill {
                bootstrap_port: Some(8000),
            },
            "m1",
            Some("v10"),
        );
        let d_v10 = make_test_worker("http://d1", WorkerType::Decode, "m1", Some("v10"));
        let d_v11 = make_test_worker("http://d2", WorkerType::Decode, "m1", Some("v11"));

        assert_eq!(
            PdCompatibilityKey::from_worker(&p_v10),
            PdCompatibilityKey::from_worker(&d_v10)
        );
        assert_ne!(
            PdCompatibilityKey::from_worker(&p_v10),
            PdCompatibilityKey::from_worker(&d_v11)
        );

        assert!(compatible_pair(&p_v10, &d_v10, PdWeightVersionMode::Strict));
        assert!(!compatible_pair(
            &p_v10,
            &d_v11,
            PdWeightVersionMode::Strict
        ));
        assert!(compatible_pair(&p_v10, &d_v11, PdWeightVersionMode::Legacy));
    }

    #[tokio::test]
    async fn test_hash_semi_join_plans_matching_pair() {
        let p_v10 = make_test_worker(
            "http://p1",
            WorkerType::Prefill {
                bootstrap_port: Some(8000),
            },
            "m1",
            Some("v10"),
        );
        let p_v11 = make_test_worker(
            "http://p2",
            WorkerType::Prefill {
                bootstrap_port: Some(8001),
            },
            "m1",
            Some("v11"),
        );
        let d_v11 = make_test_worker("http://d1", WorkerType::Decode, "m1", Some("v11"));

        let policy = RandomPolicy::new();
        let select_info = SelectWorkerInfo::default();
        let bound = OnceLock::new();

        let (sel_p, sel_d) = PdPairPlanner::plan(PdPlanParams {
            prefill_workers: vec![p_v10, p_v11],
            decode_workers: vec![d_v11],
            prefill_policy: &policy,
            decode_policy: &policy,
            mode: PdWeightVersionMode::Strict,
            model_id: Some("m1"),
            bound_weight_version: Some(&bound),
            select_info: &select_info,
            required_connection_mode: None,
        })
        .await
        .expect("should select compatible pair");

        assert_eq!(weight_version(&sel_p), Some("v11"));
        assert_eq!(weight_version(&sel_d), Some("v11"));
        assert_eq!(bound.get().and_then(|o| o.as_deref()), Some("v11"));
    }
}
