use super::*;
use crate::config::TaskTimerConfig;
use crate::session::Submission;
use crate::session::handlers::submission_loop;
use crate::session::tests::HeldStepTask;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_selected_settings_for_test;
use crate::session::tests::update_turn_settings_for_test;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::TryLockError;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn timer(action: TaskTimerAction) -> TaskTimerConfig {
    TaskTimerConfig {
        at: Utc::now(),
        action,
        model: None,
        reasoning_effort: None,
    }
}

fn shutdown_submission() -> Submission {
    Submission {
        id: "shutdown".to_string(),
        op: Op::Shutdown,
        turn_extension_init: None,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
        residency_guard: None,
    }
}

fn assert_no_settings_event(events: &async_channel::Receiver<Event>) {
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.msg, EventMsg::ThreadSettingsApplied(_)));
    }
}

async fn fixture(
    supports_fast: bool,
) -> (
    Arc<Session>,
    Arc<TurnContext>,
    async_channel::Receiver<Event>,
) {
    let (mut session, mut turn) = make_session_and_context().await;
    session
        .features
        .enable(Feature::FastMode)
        .expect("Fast mode feature");
    session
        .features
        .disable(Feature::StepModelSwitching)
        .expect("disable model switching");
    update_turn_settings_for_test(&mut turn, |settings| {
        let model = Arc::make_mut(&mut settings.model_info);
        model.service_tiers.clear();
        if supports_fast {
            model.service_tiers.push(ModelServiceTier {
                id: ServiceTier::Fast.request_value().to_string(),
                name: "Fast".to_string(),
                description: "Fast mode".to_string(),
            });
        }
    });
    session.services.models_manager = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![turn.model_info().as_ref().clone()],
        },
    ));
    let (tx_event, rx_event) = async_channel::unbounded();
    session.tx_event = tx_event;
    (Arc::new(session), Arc::new(turn), rx_event)
}

async fn next_event(
    events: &async_channel::Receiver<Event>,
    predicate: impl Fn(&EventMsg) -> bool,
) -> Event {
    timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("event channel");
            if predicate(&event.msg) {
                return event;
            }
        }
    })
    .await
    .expect("timer event")
}

const SWITCH_MODEL: &str = "task-timer-target";

fn switch_timer(model: Option<&str>, effort: Option<ReasoningEffort>) -> TaskTimerConfig {
    TaskTimerConfig {
        model: model.map(str::to_string),
        reasoning_effort: effort,
        ..timer(TaskTimerAction::Switch)
    }
}

async fn switch_fixture(
    enable_switching: bool,
) -> (
    Arc<Session>,
    Arc<TurnContext>,
    async_channel::Receiver<Event>,
) {
    let (mut session, mut turn, events) = fixture(true).await;
    let mutable = Arc::get_mut(&mut session).expect("unique session");
    if enable_switching {
        mutable
            .features
            .enable(Feature::StepModelSwitching)
            .expect("enable switching");
    }
    update_turn_settings_for_test(Arc::get_mut(&mut turn).expect("unique turn"), |settings| {
        update_selected_settings_for_test(settings, |selected| {
            selected.collaboration_mode.settings.reasoning_effort = Some(ReasoningEffort::High);
        });
        let model = Arc::make_mut(&mut settings.model_info);
        model.used_fallback_model_metadata = false;
        model.default_reasoning_level = Some(ReasoningEffort::High);
        model.supported_reasoning_levels = [ReasoningEffort::Low, ReasoningEffort::High]
            .into_iter()
            .map(|effort| ReasoningEffortPreset {
                effort,
                description: String::new(),
            })
            .collect();
    });
    let original = turn.model_info().as_ref().clone();
    let mut destination = original.clone();
    destination.slug = SWITCH_MODEL.to_string();
    let mut restricted = destination.clone();
    restricted.slug = "task-timer-restricted".to_string();
    restricted.model_specialty = Some("cyber".to_string());
    mutable.services.models_manager = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![original, destination, restricted],
        },
    ));
    let configuration = &mut mutable.state.get_mut().session_configuration;
    Arc::make_mut(&mut configuration.step_settings)
        .collaboration_mode
        .settings
        .reasoning_effort = Some(ReasoningEffort::High);
    (session, turn, events)
}

#[tokio::test]
async fn task_timer_switches_model_and_effort_during_task_and_updates_future_turns() {
    let (session, turn, events) = switch_fixture(true).await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    let before = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("before");
    let last_started = session.state.lock().await.last_started_turn_id.clone();
    let mut config = turn.config.as_ref().clone();
    let mut timer = switch_timer(Some(SWITCH_MODEL), Some(ReasoningEffort::Low));
    timer.at = Utc::now() + chrono::Duration::milliseconds(60);
    config.task_timer = Some(timer);
    let (submissions, receiver) = async_channel::unbounded::<Submission>();
    let handle = tokio::spawn(submission_loop(
        Arc::clone(&session),
        Arc::new(config),
        receiver,
    ));
    let event = next_event(&events, |event| {
        matches!(event, EventMsg::ThreadSettingsApplied(_))
    })
    .await;
    let EventMsg::ThreadSettingsApplied(event) = event.msg else {
        unreachable!()
    };
    assert_eq!(event.thread_settings.model, SWITCH_MODEL);
    assert_eq!(
        event.thread_settings.reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    let after = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("after");
    assert_ne!(before.settings.model_info.slug, SWITCH_MODEL);
    assert_eq!(
        before.settings.reasoning_effort(),
        Some(&ReasoningEffort::High)
    );
    assert_eq!(after.settings.model_info.slug, SWITCH_MODEL);
    assert_eq!(
        after.settings.reasoning_effort(),
        Some(&ReasoningEffort::Low)
    );
    assert_eq!(
        session.state.lock().await.last_started_turn_id,
        last_started
    );
    session.interrupt_task().await;
    let next = session
        .new_turn_with_default_settings("after-switch".to_string(), Default::default())
        .await;
    assert_eq!(next.initial_settings.model_info.slug, SWITCH_MODEL);
    assert_eq!(
        next.initial_settings.reasoning_effort(),
        Some(&ReasoningEffort::Low)
    );
    submissions
        .send(shutdown_submission())
        .await
        .expect("shutdown");
    timeout(Duration::from_secs(10), handle)
        .await
        .expect("shutdown loop")
        .expect("loop");
}

#[tokio::test]
async fn task_timer_switch_effort_only_preserves_model_and_pinned_metadata() {
    let (session, turn, _) = switch_fixture(true).await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    let before = turn.next_step_settings.load_full();
    session
        .apply_timed_model_settings(&switch_timer(None, Some(ReasoningEffort::Low)))
        .await
        .expect("switch effort");
    let after = turn.next_step_settings.load_full();
    assert!(Arc::ptr_eq(&before.model_info, &after.model_info));
    assert_eq!(after.reasoning_effort(), Some(&ReasoningEffort::Low));
    assert_eq!(
        session.thread_settings_snapshot().await.model,
        before.model_info.slug
    );
    session.interrupt_task().await;
}

#[tokio::test]
async fn task_timer_switch_model_only_while_idle_preserves_effort_without_feature_gate() {
    let (session, _, _) = switch_fixture(false).await;
    session
        .apply_timed_model_settings(&switch_timer(Some(SWITCH_MODEL), None))
        .await
        .expect("idle switch");
    let settings = session.thread_settings_snapshot().await;
    assert_eq!(settings.model, SWITCH_MODEL);
    assert_eq!(settings.reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn task_timer_switch_rejection_preserves_active_and_future_settings() {
    for (enabled, model, effort, expected) in [
        (
            false,
            SWITCH_MODEL,
            ReasoningEffort::Low,
            "step_model_switching",
        ),
        (
            true,
            "task-timer-not-in-catalog",
            ReasoningEffort::Low,
            "catalog metadata",
        ),
        (
            true,
            SWITCH_MODEL,
            ReasoningEffort::Ultra,
            "does not support reasoning_effort",
        ),
        (
            true,
            "task-timer-restricted",
            ReasoningEffort::Low,
            "admitted",
        ),
    ] {
        let (session, turn, _) = switch_fixture(enabled).await;
        session
            .spawn_task(
                Arc::clone(&turn),
                Vec::new(),
                HeldStepTask {
                    kind: TaskKind::Regular,
                    finish: Arc::new(Notify::new()),
                },
            )
            .await;
        let before = turn.next_step_settings.load_full();
        let defaults = session.thread_settings_snapshot().await;
        let error = session
            .apply_timed_model_settings(&switch_timer(Some(model), Some(effort)))
            .await
            .expect_err("reject invalid switch");
        assert!(error.contains(expected), "unexpected rejection: {error}");
        assert!(Arc::ptr_eq(&before, &turn.next_step_settings.load_full()));
        assert_eq!(session.thread_settings_snapshot().await, defaults);
        assert!(session.active_turn.lock().await.is_some());
        session.interrupt_task().await;
    }
}

#[derive(Debug)]
struct GatedTimerModels {
    inner: Arc<dyn ModelsManager>,
    gate_next: AtomicBool,
    started: Notify,
    release: Notify,
}

impl ModelsManager for GatedTimerModels {
    fn refresh_if_new_etag(
        &self,
        etag: String,
        client: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        self.inner.refresh_if_new_etag(etag, client)
    }

    fn raw_model_catalog(
        &self,
        strategy: RefreshStrategy,
        client: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        self.inner.raw_model_catalog(strategy, client)
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        self.inner.get_remote_models()
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.inner.try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(async move {
            if self.gate_next.swap(false, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            self.inner.get_model_info(model, config).await
        })
    }
}

#[tokio::test]
async fn task_timer_switch_does_not_retarget_replacement_task_after_delayed_lookup() {
    let (mut session, turn, _) = switch_fixture(true).await;
    let gated = Arc::new(GatedTimerModels {
        inner: Arc::clone(&session.services.models_manager),
        gate_next: AtomicBool::new(true),
        started: Notify::new(),
        release: Notify::new(),
    });
    Arc::get_mut(&mut session)
        .expect("unique session")
        .services
        .models_manager = gated.clone();
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    let before = turn.next_step_settings.load_full();
    let defaults = session.thread_settings_snapshot().await;
    let switching = Arc::clone(&session);
    let handle = tokio::spawn(async move {
        switching
            .apply_timed_model_settings(&switch_timer(
                Some(SWITCH_MODEL),
                Some(ReasoningEffort::Low),
            ))
            .await
    });
    timeout(Duration::from_secs(10), gated.started.notified())
        .await
        .expect("lookup started");
    // Reuse the exact context and turn ID, but register a different task. The
    // timer must also match its per-task completion signal before publishing.
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    gated.release.notify_one();
    let error = timeout(Duration::from_secs(10), handle)
        .await
        .expect("lookup finished")
        .expect("switch task")
        .expect_err("replacement rejected");
    assert!(error.contains("running task"));
    assert!(Arc::ptr_eq(&before, &turn.next_step_settings.load_full()));
    assert_eq!(session.thread_settings_snapshot().await, defaults);
    assert!(session.active_turn.lock().await.is_some());
    session.interrupt_task().await;
}

#[tokio::test]
async fn task_timer_fast_changes_next_step_and_future_turn_without_replacing_running_request() {
    let (session, turn, events) = fixture(true).await;
    let finish = Arc::new(Notify::new());
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish,
            },
        )
        .await;
    let before = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("capture before");
    let last_started = session.state.lock().await.last_started_turn_id.clone();
    let mut config = turn.config.as_ref().clone();
    config.task_timer = Some(TaskTimerConfig {
        at: Utc::now() + chrono::Duration::milliseconds(60),
        action: TaskTimerAction::Fast,
        model: None,
        reasoning_effort: None,
    });
    let (submissions, receiver) = async_channel::unbounded::<Submission>();
    let task = tokio::spawn(submission_loop(
        Arc::clone(&session),
        Arc::new(config),
        receiver,
    ));
    let notification = next_event(&events, |event| {
        matches!(event, EventMsg::ThreadSettingsApplied(_))
    })
    .await;
    let after = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("capture after");
    assert_eq!(before.settings.service_tier, None);
    assert_eq!(after.settings.service_tier.as_deref(), Some("priority"));
    assert!(Arc::ptr_eq(
        &before.settings.model_info,
        &after.settings.model_info
    ));
    assert_eq!(
        session.state.lock().await.last_started_turn_id,
        last_started
    );
    assert!(session.active_turn.lock().await.is_some());
    let EventMsg::ThreadSettingsApplied(notification) = notification.msg else {
        unreachable!()
    };
    assert_eq!(
        notification.thread_settings.service_tier.as_deref(),
        Some("priority")
    );
    session.interrupt_task().await;
    let next = session
        .new_turn_with_default_settings("next".to_string(), Default::default())
        .await;
    assert_eq!(
        next.initial_settings.service_tier.as_deref(),
        Some("priority")
    );
    submissions
        .send(shutdown_submission())
        .await
        .expect("shutdown submission");
    timeout(Duration::from_secs(10), task)
        .await
        .expect("loop shutdown")
        .expect("loop task");
}

#[tokio::test]
async fn task_timer_fast_rejects_unsupported_model_without_changing_settings() {
    let (session, turn, events) = fixture(false).await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    let before = turn.next_step_settings.load_full();
    fire(&session, &timer(TaskTimerAction::Fast)).await;
    let warning = next_event(&events, |event| matches!(event, EventMsg::Warning(_))).await;
    let EventMsg::Warning(warning) = warning.msg else {
        unreachable!()
    };
    assert!(warning.message.contains("does not support Fast mode"));
    assert!(Arc::ptr_eq(&before, &turn.next_step_settings.load_full()));
    assert_eq!(session.thread_settings_snapshot().await.service_tier, None);
    session.interrupt_task().await;
}

#[tokio::test]
async fn task_timer_fast_rejects_unsupported_running_model_without_partial_update() {
    let (session, mut turn, _) = fixture(true).await;
    update_turn_settings_for_test(Arc::get_mut(&mut turn).expect("unique turn"), |settings| {
        Arc::make_mut(&mut settings.model_info)
            .service_tiers
            .clear();
    });
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    let before = turn.next_step_settings.load_full();
    let error = session
        .enable_timed_fast_mode()
        .await
        .expect_err("running model lacks Fast");
    assert!(error.contains("running model"));
    assert!(Arc::ptr_eq(&before, &turn.next_step_settings.load_full()));
    assert_eq!(session.thread_settings_snapshot().await.service_tier, None);
    session.interrupt_task().await;
}

#[tokio::test]
async fn task_timer_fast_respects_disabled_feature() {
    let (mut session, _, _) = fixture(true).await;
    Arc::get_mut(&mut session)
        .expect("unique session")
        .features
        .disable(Feature::FastMode)
        .expect("disable Fast");
    let error = session
        .enable_timed_fast_mode()
        .await
        .expect_err("feature disabled");
    assert!(error.contains("disabled"));
    assert_eq!(session.thread_settings_snapshot().await.service_tier, None);
}

#[tokio::test]
async fn task_timer_stop_uses_normal_task_interruption() {
    let (session, turn, events) = fixture(true).await;
    session
        .spawn_task(
            turn,
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    fire(&session, &timer(TaskTimerAction::Stop)).await;
    let event = next_event(&events, |event| matches!(event, EventMsg::TurnAborted(_))).await;
    let EventMsg::TurnAborted(event) = event.msg else {
        unreachable!()
    };
    assert_eq!(event.reason, TurnAbortReason::Interrupted);
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn task_timer_fires_while_idle_only_once() {
    let (session, turn, events) = fixture(true).await;
    let mut config = turn.config.as_ref().clone();
    config.task_timer = Some(TaskTimerConfig {
        at: Utc::now() + chrono::Duration::milliseconds(60),
        action: TaskTimerAction::Fast,
        model: None,
        reasoning_effort: None,
    });
    let (submissions, receiver) = async_channel::unbounded::<Submission>();
    let task = tokio::spawn(submission_loop(
        Arc::clone(&session),
        Arc::new(config),
        receiver,
    ));
    next_event(&events, |event| matches!(event, EventMsg::Warning(warning) if warning.message.contains("enabled Fast"))).await;
    assert_eq!(
        session
            .thread_settings_snapshot()
            .await
            .service_tier
            .as_deref(),
        Some("priority")
    );
    session
        .update_settings(SessionSettingsUpdate {
            step_settings: StepSettingsUpdate {
                service_tier: Some(Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("manual override after timer");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        session
            .thread_settings_snapshot()
            .await
            .service_tier
            .as_deref(),
        Some("default")
    );
    submissions
        .send(shutdown_submission())
        .await
        .expect("shutdown submission");
    timeout(Duration::from_secs(10), task)
        .await
        .expect("loop shutdown")
        .expect("loop task");
}

#[tokio::test]
async fn task_timer_shutdown_cancels_wait() {
    let (session, turn, events) = fixture(true).await;
    let mut config = turn.config.as_ref().clone();
    config.task_timer = Some(TaskTimerConfig {
        at: Utc::now() + chrono::Duration::hours(1),
        action: TaskTimerAction::Fast,
        model: None,
        reasoning_effort: None,
    });
    let (submissions, receiver) = async_channel::unbounded::<Submission>();
    let task = tokio::spawn(submission_loop(
        Arc::clone(&session),
        Arc::new(config),
        receiver,
    ));
    submissions
        .send(shutdown_submission())
        .await
        .expect("shutdown submission");
    timeout(Duration::from_secs(10), task)
        .await
        .expect("loop shutdown")
        .expect("loop task");
    assert_eq!(session.thread_settings_snapshot().await.service_tier, None);
    assert_no_settings_event(&events);
}

#[tokio::test]
async fn task_timer_does_not_run_in_subagent_session() {
    let (session, turn, events) = fixture(true).await;
    session
        .state
        .lock()
        .await
        .session_configuration
        .session_source = SessionSource::SubAgent(SubAgentSource::Review);
    let mut config = turn.config.as_ref().clone();
    config.task_timer = Some(TaskTimerConfig {
        at: Utc::now() - chrono::Duration::seconds(1),
        action: TaskTimerAction::Fast,
        model: None,
        reasoning_effort: None,
    });
    let (submissions, receiver) = async_channel::unbounded::<Submission>();
    let task = tokio::spawn(submission_loop(
        Arc::clone(&session),
        Arc::new(config),
        receiver,
    ));
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(session.thread_settings_snapshot().await.service_tier, None);
    submissions
        .send(shutdown_submission())
        .await
        .expect("shutdown submission");
    timeout(Duration::from_secs(10), task)
        .await
        .expect("loop shutdown")
        .expect("loop task");
    assert_no_settings_event(&events);
}

#[tokio::test]
async fn task_timer_wait_observes_deadline_and_expired_time() {
    let at = Utc::now() + chrono::Duration::milliseconds(60);
    wait_until(at).await;
    assert!(Utc::now() >= at);
    timeout(
        Duration::from_millis(50),
        wait_until(Utc::now() - chrono::Duration::hours(1)),
    )
    .await
    .expect("expired timer fires immediately");
}
