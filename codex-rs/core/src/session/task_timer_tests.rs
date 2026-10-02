use super::*;
use crate::config::TaskTimerConfig;
use crate::session::Submission;
use crate::session::handlers::submission_loop;
use crate::session::tests::HeldStepTask;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_turn_settings_for_test;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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
    fire(&session, TaskTimerAction::Fast).await;
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
    fire(&session, TaskTimerAction::Stop).await;
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
