//! Engine-level realtime behaviour with a fake repository and adapter (no
//! database or network): capability gating before admission, accounting of a
//! failed connect, and synchronous upstream cancellation on drop.
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures_util::{SinkExt, stream};

use super::*;
use crate::{
    inference::{repository::ExecutionFinish, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
};

#[derive(Default)]
struct Repo {
    response_gate: StdMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    settlement_events: StdMutex<Vec<&'static str>>,
    admitted: AtomicUsize,
    finished: StdMutex<Vec<(Uuid, Outcome, Option<InferenceError>)>>,
    /// Routes of the model (default: one, `Uuid::nil()`), in plan order.
    routes: StdMutex<Vec<Uuid>>,
    /// The plan's attempts and failover policy (default: one attempt).
    plan: StdMutex<Option<(usize, bool)>>,
    /// Routes admission refuses as drained (`routing::drain`).
    drained: StdMutex<Vec<Uuid>>,
    /// Routes admitted, in order.
    admitted_routes: StdMutex<Vec<Uuid>>,
}
#[async_trait]
impl InferenceRepository for Repo {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>, InferenceError> {
        let routes = self.routes.lock().unwrap().clone();
        let routes = if routes.is_empty() {
            vec![Uuid::nil()]
        } else {
            routes
        };
        Ok(routes
            .into_iter()
            .map(|id| Deployment {
                image_input: false,
                id,
                provider: "fake".into(),
                upstream_model: "upstream".into(),
                credential_ref: "env:X".into(),
                endpoint: None,
                region: None,
                supported_protocols: vec!["realtime".into()],
            })
            .collect())
    }
    async fn route_plan(
        &self,
        _: &Principal,
        _: &str,
        candidates: &[Deployment],
        _: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        let (max_attempts, allow_ambiguous_failover) =
            self.plan.lock().unwrap().unwrap_or((1, false));
        Ok(crate::routing::RoutePlan {
            max_attempts,
            allow_ambiguous_failover,
            ..crate::routing::RoutePlan::single(candidates.iter().map(|d| d.id).collect())
        })
    }
    /// A fixed list: there is no cache to bypass.
    async fn live_candidates(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError> {
        self.deployments(principal, model).await
    }
    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError> {
        if self.drained.lock().unwrap().contains(&record.deployment_id) {
            return Err(InferenceError::RouteDrained(None));
        }
        self.admitted.fetch_add(1, Ordering::SeqCst);
        self.admitted_routes
            .lock()
            .unwrap()
            .push(record.deployment_id);
        Ok(())
    }
    async fn realtime_settle_response(
        &self,
        _: Uuid,
        _: i32,
        _: Option<ResponseStatus>,
        _: Option<RealtimeUsage>,
        _: ResponseBound,
    ) -> Result<(), InferenceError> {
        self.settlement_events
            .lock()
            .unwrap()
            .push("response entered");
        let gate = self.response_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.await.unwrap();
        }
        self.settlement_events
            .lock()
            .unwrap()
            .push("response completed");
        Ok(())
    }
    async fn finish(&self, _: &ExecutionFinish) -> Result<(), InferenceError> {
        panic!("realtime sessions must use realtime_finish")
    }
    async fn realtime_finish(&self, record: &RealtimeFinish) -> Result<(), InferenceError> {
        self.settlement_events
            .lock()
            .unwrap()
            .push("finish entered");
        self.finished
            .lock()
            .unwrap()
            .push((record.id, record.outcome, record.error));
        Ok(())
    }
}

struct Signal(Arc<AtomicBool>);
impl Drop for Signal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct Fake {
    realtime: bool,
    fail: Option<InferenceError>,
    dropped: Arc<AtomicBool>,
}
/// Routes on which [`Fake`] fails to connect, whatever its `fail`.
static FAIL_ROUTES: std::sync::LazyLock<StdMutex<Vec<Uuid>>> =
    std::sync::LazyLock::new(StdMutex::default);
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            image_input: false,
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        self.realtime && protocol == ApiProtocol::Realtime
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn connect_realtime(
        &self,
        deployment: &Deployment,
        setup: &RealtimeSetup,
    ) -> Result<RealtimeUpstream, InferenceError> {
        assert_eq!(setup.window_output_tokens, 512);
        if let Some(error) = self.fail {
            return Err(error);
        }
        if FAIL_ROUTES.lock().unwrap().contains(&deployment.id) {
            return Err(InferenceError::UpstreamUnavailable);
        }
        let signal = Signal(self.dropped.clone());
        Ok(RealtimeUpstream {
            sink: Box::pin(
                futures_util::sink::drain::<String>()
                    .sink_map_err(|never: std::convert::Infallible| match never {}),
            ),
            events: Box::pin(stream::poll_fn(move |_| {
                let _held = &signal;
                std::task::Poll::Pending
            })),
            config: Default::default(),
        })
    }
}
fn engine(repo: Arc<Repo>, adapter: Fake) -> Engine {
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(adapter)).unwrap();
    Engine::new(repo, registry, EngineLimits::default())
        .unwrap()
        .with_realtime_limits(RealtimeLimits {
            max_output_tokens: 512,
            ..RealtimeLimits::default()
        })
        .unwrap()
}
fn principal() -> Principal {
    Principal {
        key_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        user_id: None,
    }
}

#[tokio::test]
async fn unsupported_adapters_fail_before_admission() {
    let repo = Arc::new(Repo::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let e = engine(
        repo.clone(),
        Fake {
            realtime: false,
            fail: None,
            dropped,
        },
    );
    let result = e
        .open_realtime(principal(), "company/voice", Uuid::new_v4())
        .await;
    assert_eq!(result.err(), Some(InferenceError::Unsupported));
    assert_eq!(
        e.open_realtime(principal(), " ", Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::InvalidRequest)
    );
    assert_eq!(repo.admitted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_connect_is_an_accounted_failed_attempt() {
    let repo = Arc::new(Repo::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let e = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: Some(InferenceError::UpstreamRejected),
            dropped,
        },
    );
    let id = Uuid::new_v4();
    assert_eq!(
        e.open_realtime(principal(), "company/voice", id)
            .await
            .err(),
        Some(InferenceError::UpstreamRejected)
    );
    assert_eq!(repo.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(
        repo.finished.lock().unwrap().as_slice(),
        [(id, Outcome::Failed, Some(InferenceError::UpstreamRejected))]
    );
}

#[tokio::test]
async fn dropping_a_session_closes_upstream_now_and_records_a_cancellation() {
    let repo = Arc::new(Repo::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let e = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: None,
            dropped: dropped.clone(),
        },
    );
    let id = Uuid::new_v4();
    let session = e
        .open_realtime(principal(), "company/voice", id)
        .await
        .unwrap();
    assert_eq!(session.window_output_tokens(), 512);
    assert!(!dropped.load(Ordering::SeqCst));
    drop(session);
    // Synchronously, before any accounting task runs.
    assert!(dropped.load(Ordering::SeqCst));
    for _ in 0..100 {
        if !repo.finished.lock().unwrap().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        repo.finished.lock().unwrap().as_slice(),
        [(id, Outcome::Cancelled, None)]
    );
}

#[tokio::test(start_paused = true)]
async fn final_write_waits_for_cancelled_response_settlement() {
    let (release, gate) = tokio::sync::oneshot::channel();
    let repo = Arc::new(Repo {
        response_gate: StdMutex::new(Some(gate)),
        ..Repo::default()
    });
    let id = Uuid::new_v4();
    let mut session = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: None,
            dropped: Arc::default(),
        },
    )
    .open_realtime(principal(), "company/voice", id)
    .await
    .unwrap();
    session.open_response("r1".into()).await.unwrap();
    {
        let mut response =
            Box::pin(session.settle_response("r1", Some(ResponseStatus::Completed), None));
        assert!(futures_util::poll!(&mut response).is_pending());
    }

    // The response owner has handed off its pending write. Poll finish on this
    // task so it has a chance to enter the repository while the gate is shut.
    let mut finish = Box::pin(session.finish(Outcome::Cancelled, None, None));
    let first_poll = futures_util::poll!(&mut finish);
    assert_eq!(
        *repo.settlement_events.lock().unwrap(),
        ["response entered"]
    );
    assert!(first_poll.is_pending());

    release.send(()).unwrap();
    finish.await.unwrap();
    assert_eq!(
        *repo.settlement_events.lock().unwrap(),
        ["response entered", "response completed", "finish entered"]
    );
    assert_eq!(
        repo.finished.lock().unwrap().as_slice(),
        [(id, Outcome::Cancelled, None)]
    );
}

/// The first planned route was drained after the plan was read (another
/// replica's health cache): admission refuses it, and the session opens on
/// the second route as its one attempt.
#[tokio::test]
async fn a_session_passes_over_a_route_drained_at_admission() {
    let repo = Arc::new(Repo::default());
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    *repo.routes.lock().unwrap() = vec![first, second];
    repo.drained.lock().unwrap().push(first);
    let dropped = Arc::new(AtomicBool::new(false));
    let e = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: None,
            dropped,
        },
    );
    let session = e
        .open_realtime(principal(), "company/voice", Uuid::new_v4())
        .await
        .expect("opens on the second route");
    assert_eq!(*repo.admitted_routes.lock().unwrap(), vec![second]);
    drop(session);
    // Every route drained: temporarily unavailable, never a generic Busy.
    let repo = Arc::new(Repo::default());
    *repo.routes.lock().unwrap() = vec![first, second];
    repo.drained.lock().unwrap().extend([first, second]);
    let e = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: None,
            dropped: Arc::new(AtomicBool::new(false)),
        },
    );
    assert_eq!(
        e.open_realtime(principal(), "company/voice", Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::RouteDrained(None))
    );
}

/// The first attempt fails over; the only route left is refused as drained:
/// the request ends with the first attempt's own error, not a generic Busy.
#[tokio::test]
async fn a_failover_onto_a_drained_route_keeps_the_first_attempts_error() {
    let repo = Arc::new(Repo::default());
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    *repo.routes.lock().unwrap() = vec![first, second];
    *repo.plan.lock().unwrap() = Some((2, true));
    repo.drained.lock().unwrap().push(second);
    FAIL_ROUTES.lock().unwrap().push(first);
    let e = engine(
        repo.clone(),
        Fake {
            realtime: true,
            fail: None,
            dropped: Arc::new(AtomicBool::new(false)),
        },
    );
    assert_eq!(
        e.open_realtime(principal(), "company/voice", Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::UpstreamUnavailable)
    );
    assert_eq!(*repo.admitted_routes.lock().unwrap(), vec![first]);
}
