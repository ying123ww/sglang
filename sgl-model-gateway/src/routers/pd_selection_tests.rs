//! Run the same selection contract against both production selectors.
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use http::HeaderMap;

use super::{
    grpc::common::stages::{WorkerSelectionMode, WorkerSelectionStage},
    http::pd_router::PDRouter,
};
use crate::{
    config::{PdWeightVersionMode, PolicyConfig, RetryConfig},
    core::{
        BasicWorkerBuilder, ConnectionMode, ModelCard, Worker, WorkerRegistry, WorkerType,
        UNKNOWN_MODEL_ID,
    },
    policies::{LoadBalancingPolicy, PolicyRegistry, SelectWorkerInfo},
};

#[derive(Clone, Copy, Debug)]
enum Transport {
    Http,
    Grpc,
}

impl Transport {
    fn connection(self) -> ConnectionMode {
        match self {
            Self::Http => ConnectionMode::Http,
            Self::Grpc => ConnectionMode::Grpc { port: None },
        }
    }
}

struct Harness {
    transport: Transport,
    http: PDRouter,
    grpc: WorkerSelectionStage,
}

impl Harness {
    fn new(transport: Transport, mode: PdWeightVersionMode, policy: PolicyConfig) -> Self {
        let registry = Arc::new(WorkerRegistry::new());
        let policies = Arc::new(PolicyRegistry::new(policy));
        Self {
            transport,
            grpc: WorkerSelectionStage::new(
                registry.clone(),
                policies.clone(),
                WorkerSelectionMode::PrefillDecode,
            )
            .with_pd_weight_version_mode(mode),
            http: PDRouter {
                worker_registry: registry,
                policy_registry: policies,
                client: reqwest::Client::new(),
                retry_config: RetryConfig::default(),
                api_key: None,
                enable_igw: false,
                pd_weight_version_mode: mode,
            },
        }
    }

    fn worker(
        &self,
        url: &str,
        role: WorkerType,
        model: &str,
        version: Option<&str>,
    ) -> BasicWorkerBuilder {
        let builder = BasicWorkerBuilder::new(url)
            .worker_type(role)
            .connection_mode(self.transport.connection())
            .model(ModelCard::new(model));
        match version {
            Some(v) => builder.label("weight_version", v),
            None => builder,
        }
    }

    fn add(&self, builder: BasicWorkerBuilder) -> Arc<dyn Worker> {
        let worker: Arc<dyn Worker> = Arc::new(builder.build());
        self.http.worker_registry.register(worker.clone());
        worker
    }

    async fn select(
        &self,
        model: Option<&str>,
        headers: Option<&HeaderMap>,
        bound: Option<&OnceLock<Option<String>>>,
    ) -> Result<(Arc<dyn Worker>, Arc<dyn Worker>), String> {
        match self.transport {
            Transport::Http => {
                self.http
                    .select_pd_pair_with_bound(None, model, headers, bound)
                    .await
            }
            Transport::Grpc => self
                .grpc
                .select_pd_pair_with_bound(model, None, None, headers, bound)
                .await
                .ok_or_else(|| "No compatible pair or policy target".to_string()),
        }
    }
}

fn prefill() -> WorkerType {
    WorkerType::Prefill {
        bootstrap_port: Some(8998),
    }
}

#[tokio::test]
async fn test_pd_weight_version_model_source_and_scope_must_match() {
    for transport in [Transport::Http, Transport::Grpc] {
        for (p_model, d_model, key, p_value, d_value) in [
            ("qwen", "other", "model_path", None, None),
            (UNKNOWN_MODEL_ID, UNKNOWN_MODEL_ID, "model_path", None, None),
            ("", "", "model_path", None, None),
            ("default", "default", "model_path", None, None),
            ("qwen", "qwen", "model_path", Some("/a"), Some("/b")),
            ("qwen", "qwen", "model_path", Some("/a"), None),
            ("qwen", "qwen", "deployment_scope", Some("a"), Some("b")),
            ("qwen", "qwen", "deployment_scope", Some("a"), None),
            ("qwen", "qwen", "deployment_scope", Some(" "), Some(" ")),
        ] {
            let h = Harness::new(
                transport,
                PdWeightVersionMode::Strict,
                PolicyConfig::RoundRobin,
            );
            let mut p = h.worker("http://p:30000", prefill(), p_model, Some("v11"));
            let mut d = h.worker("http://d:30001", WorkerType::Decode, d_model, Some("v11"));
            if let Some(value) = p_value {
                p = p.label(key, value);
            }
            if let Some(value) = d_value {
                d = d.label(key, value);
            }
            h.add(p);
            h.add(d);
            assert!(
                h.select(None, None, None).await.is_err(),
                "{transport:?}: {p_model}/{d_model} {key}: {p_value:?}/{d_value:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_pd_weight_version_unknown_versions_rejected_by_both_selectors() {
    for transport in [Transport::Http, Transport::Grpc] {
        for (p, d) in [
            (None, None),
            (None, Some("v11")),
            (Some("v11"), None),
            (Some(""), Some("")),
            (Some(" "), Some(" ")),
        ] {
            let h = Harness::new(
                transport,
                PdWeightVersionMode::Strict,
                PolicyConfig::RoundRobin,
            );
            h.add(h.worker("http://p:30000", prefill(), "qwen", p));
            h.add(h.worker("http://d:30001", WorkerType::Decode, "qwen", d));
            assert!(
                h.select(Some("qwen"), None, None).await.is_err(),
                "{transport:?}: {p:?}/{d:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_pd_weight_version_ambiguous_model_pool_requires_request_identity() {
    for transport in [Transport::Http, Transport::Grpc] {
        let h = Harness::new(
            transport,
            PdWeightVersionMode::Strict,
            PolicyConfig::RoundRobin,
        );
        for (model, port) in [("qwen", 30000), ("other", 30002)] {
            h.add(h.worker(&format!("http://p:{port}"), prefill(), model, Some("v11")));
            h.add(h.worker(
                &format!("http://d:{}", port + 1),
                WorkerType::Decode,
                model,
                Some("v11"),
            ));
        }
        assert!(h.select(None, None, None).await.is_err());
        let (p, d) = h.select(Some("qwen"), None, None).await.unwrap();
        assert_eq!((p.model_id(), d.model_id()), ("qwen", "qwen"));
        assert!(h.select(Some("unregistered"), None, None).await.is_err());
    }
}

#[tokio::test]
async fn test_pd_weight_version_ambiguous_deployment_pool_is_rejected() {
    for transport in [Transport::Http, Transport::Grpc] {
        let h = Harness::new(
            transport,
            PdWeightVersionMode::Strict,
            PolicyConfig::RoundRobin,
        );
        for (scope, port) in [("a", 30000), ("b", 30002)] {
            h.add(
                h.worker(&format!("http://p:{port}"), prefill(), "qwen", Some("v11"))
                    .label("deployment_scope", scope),
            );
            h.add(
                h.worker(
                    &format!("http://d:{}", port + 1),
                    WorkerType::Decode,
                    "qwen",
                    Some("v11"),
                )
                .label("deployment_scope", scope),
            );
        }
        assert!(h.select(None, None, None).await.is_err());
    }
}

#[tokio::test]
async fn test_pd_weight_version_legacy_preserves_mismatch_and_retry_behavior() {
    for transport in [Transport::Http, Transport::Grpc] {
        for (p_version, d_version) in [
            (Some("v10"), Some("v11")),
            (None, Some("v11")),
            (None, None),
        ] {
            let h = Harness::new(
                transport,
                PdWeightVersionMode::Legacy,
                PolicyConfig::RoundRobin,
            );
            h.add(h.worker("http://p:30000", prefill(), "qwen", p_version));
            h.add(h.worker("http://d:30001", WorkerType::Decode, "qwen", d_version));
            let bound = OnceLock::new();
            bound.set(Some("previous-version".into())).unwrap();
            let (p, d) = h.select(Some("qwen"), None, Some(&bound)).await.unwrap();
            assert_eq!(
                p.metadata()
                    .labels
                    .get("weight_version")
                    .map(String::as_str),
                p_version
            );
            assert_eq!(
                d.metadata()
                    .labels
                    .get("weight_version")
                    .map(String::as_str),
                d_version
            );
            assert_eq!(bound.get().unwrap().as_deref(), Some("previous-version"));
        }
    }
    let h = Harness::new(
        Transport::Http,
        PdWeightVersionMode::Legacy,
        PolicyConfig::RoundRobin,
    );
    h.add(h.worker("http://p:30000", prefill(), "qwen", Some("v10")));
    h.add(h.worker("http://d:30001", WorkerType::Decode, "other", Some("v11")));
    // Original non-IGW behavior ignores the request model.
    assert!(h.select(Some("unregistered"), None, None).await.is_ok());
}

#[tokio::test]
async fn test_pd_weight_version_preserves_health_circuit_role_and_connection_filters() {
    for transport in [Transport::Http, Transport::Grpc] {
        let h = Harness::new(
            transport,
            PdWeightVersionMode::Strict,
            PolicyConfig::RoundRobin,
        );
        h.add(h.worker("http://unhealthy:30000", prefill(), "qwen", Some("v11")))
            .set_healthy(false);
        let blocked = h.add(h.worker("http://circuit:30002", prefill(), "qwen", Some("v11")));
        for _ in 0..5 {
            blocked.circuit_breaker().record_failure();
        }
        assert!(!blocked.is_available());
        h.add(h.worker(
            "http://regular:30004",
            WorkerType::Regular,
            "qwen",
            Some("v11"),
        ));
        let wrong = match transport {
            Transport::Http => Transport::Grpc,
            Transport::Grpc => Transport::Http,
        };
        h.add(
            h.worker(
                "http://wrong-transport:30006",
                prefill(),
                "qwen",
                Some("v11"),
            )
            .connection_mode(wrong.connection()),
        );
        h.add(h.worker("http://d:30001", WorkerType::Decode, "qwen", Some("v11")));
        assert!(h.select(Some("qwen"), None, None).await.is_err());
        h.add(h.worker("http://p:30008", prefill(), "qwen", Some("v11")));
        let (p, _) = h.select(Some("qwen"), None, None).await.unwrap();
        assert_eq!(p.url(), "http://p:30008");
    }
}

#[tokio::test]
async fn test_pd_weight_version_hash_and_target_stay_inside_filtered_candidates() {
    for transport in [Transport::Http, Transport::Grpc] {
        let h = Harness::new(
            transport,
            PdWeightVersionMode::Strict,
            PolicyConfig::ConsistentHashing,
        );
        h.add(h.worker("http://excluded:30000", prefill(), "qwen", Some("v10")));
        h.add(h.worker("http://p:30002", prefill(), "qwen", Some("v11")));
        h.add(h.worker("http://d:30001", WorkerType::Decode, "qwen", Some("v11")));
        let ring = h.http.worker_registry.get_hash_ring("qwen").unwrap();
        let key = (0..1000)
            .map(|i| format!("key-{i}"))
            .find(|key| ring.find_healthy_url(key, |_| true) == Some("http://excluded:30000"))
            .expect("a routing key whose initial ring entry is excluded");
        let mut headers = HeaderMap::new();
        headers.insert("x-smg-routing-key", key.parse().unwrap());
        let (p, d) = h.select(Some("qwen"), Some(&headers), None).await.unwrap();
        assert_eq!((p.url(), d.url()), ("http://p:30002", "http://d:30001"));
        // Target indices address each policy's filtered slice, not registry order.
        headers.insert("x-smg-target-worker", "0".parse().unwrap());
        let (p, _) = h.select(Some("qwen"), Some(&headers), None).await.unwrap();
        assert_eq!(p.url(), "http://p:30002");
        for invalid in ["1", "-1", "not-an-index"] {
            headers.insert("x-smg-target-worker", invalid.parse().unwrap());
            assert!(h.select(Some("qwen"), Some(&headers), None).await.is_err());
        }
    }
}

#[derive(Debug)]
struct RecordingPolicy {
    role: &'static str,
    calls: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl LoadBalancingPolicy for RecordingPolicy {
    async fn select_worker(
        &self,
        workers: &[Arc<dyn Worker>],
        _: &SelectWorkerInfo<'_>,
    ) -> Option<usize> {
        self.calls
            .lock()
            .unwrap()
            .push(workers.iter().map(|w| w.url().into()).collect());
        assert!(workers
            .iter()
            .all(|w| w.metadata().labels["weight_version"] == "v11"));
        assert!(workers.iter().all(|w| match self.role {
            "prefill" => matches!(w.worker_type(), WorkerType::Prefill { .. }),
            _ => matches!(w.worker_type(), WorkerType::Decode),
        }));
        workers.len().checked_sub(1)
    }
    fn name(&self) -> &'static str {
        self.role
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn test_pd_weight_version_http_independent_policies_and_filtered_indices() {
    let h = Harness::new(
        Transport::Http,
        PdWeightVersionMode::Strict,
        PolicyConfig::RoundRobin,
    );
    let p_policy = Arc::new(RecordingPolicy {
        role: "prefill",
        calls: Mutex::new(vec![]),
    });
    let d_policy = Arc::new(RecordingPolicy {
        role: "decode",
        calls: Mutex::new(vec![]),
    });
    h.http.policy_registry.set_prefill_policy(p_policy.clone());
    h.http.policy_registry.set_decode_policy(d_policy.clone());
    h.add(h.worker("http://excluded:30000", prefill(), "qwen", Some("v10")));
    for (i, role) in [
        (1, prefill()),
        (2, prefill()),
        (3, WorkerType::Decode),
        (4, WorkerType::Decode),
    ] {
        h.add(h.worker(
            &format!("http://worker:{}", 30000 + i),
            role,
            "qwen",
            Some("v11"),
        ));
    }
    let (p, d) = h.select(Some("qwen"), None, None).await.unwrap();
    for (policy, selected) in [(p_policy, p), (d_policy, d)] {
        let calls = policy.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "a policy must not be invoked speculatively");
        assert_eq!(calls[0].len(), 2);
        assert_eq!(selected.url(), calls[0].last().unwrap());
    }
}
