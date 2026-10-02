//! One-shot wall-clock actions owned by the session submission loop.

use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::ResolvedStepSettings;
use super::step_settings::StepSettingsUpdate;
use super::thread_settings;
use chrono::DateTime;
use chrono::Utc;
use codex_config::config_toml::TaskTimerAction;
use codex_features::Feature;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn wait_until(at: DateTime<Utc>) {
    loop {
        let Ok(remaining) = (at - Utc::now()).to_std() else {
            return;
        };
        if remaining.is_zero() {
            return;
        }
        // Re-read wall time after clock adjustments and system sleep. Dropping
        // this future on a submission or shutdown cancels the wait.
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
    }
}

pub(super) async fn fire(session: &Arc<Session>, action: TaskTimerAction) {
    let message = match action {
        TaskTimerAction::Fast => match session.enable_timed_fast_mode().await {
            Ok(()) => "Task timer enabled Fast mode for subsequent model requests.".to_string(),
            Err(reason) => format!("Task timer could not enable Fast mode: {reason}"),
        },
        TaskTimerAction::Stop => {
            session.interrupt_task().await;
            "Task timer interrupted the current task. Automatic goal continuation is stopped."
                .to_string()
        }
    };
    session
        .send_event_raw(Event {
            id: super::new_submission_id(),
            msg: EventMsg::Warning(WarningEvent { message }),
        })
        .await;
}

impl Session {
    /// Changes only the service tier. Retained step contexts keep their tier;
    /// future captures use the published snapshot without changing the model.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize timer publication with step capture and task replacement"
    )]
    async fn enable_timed_fast_mode(&self) -> Result<(), String> {
        if !self.features.enabled(Feature::FastMode) {
            return Err("the fast_mode feature is disabled".to_string());
        }
        let _settings_guard = thread_settings::acquire_persistence_lock(self).await;
        let configuration = {
            let state = self.state.lock().await;
            if state.shutting_down {
                return Err("the session is shutting down".to_string());
            }
            state.session_configuration.clone()
        };
        let tier = ServiceTier::Fast.request_value();
        let model_info = configuration
            .step_settings
            .resolve_model_info(
                self.services.models_manager.as_ref(),
                &configuration.model_info_overrides,
            )
            .await;
        if !model_info.supports_service_tier(tier) {
            return Err(format!(
                "model `{}` does not support Fast mode",
                model_info.slug
            ));
        }

        // The same lock protects step capture and task replacement. Keep it
        // through publication so a timer cannot patch a completed task's context.
        let active = self.active_turn.lock().await;
        let task = active
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .filter(|task| !task.cancellation_token.is_cancelled());
        let next_settings = if let Some(task) = task {
            let current = task.turn_context.next_step_settings.load_full();
            if !current.model_info.supports_service_tier(tier) {
                return Err(format!(
                    "running model `{}` does not support Fast mode",
                    current.model_info.slug,
                ));
            }
            let mut selected = current.selected().clone();
            selected.service_tier = Some(tier.to_string());
            let mut next = ResolvedStepSettings::new(
                Arc::new(selected),
                Arc::clone(&current.model_info),
                /*fast_mode_enabled*/ true,
            );
            next.mcp_approvals_reviewer_override = current.mcp_approvals_reviewer_override;
            Some(next)
        } else {
            None
        };
        let current_environments = self.services.turn_environments.selections();
        let mut rejection = None;
        let commit = self
            .update_settings_if(
                SessionSettingsUpdate {
                    step_settings: StepSettingsUpdate {
                        service_tier: Some(Some(tier.to_string())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                |current, _candidate| {
                    if !Arc::ptr_eq(&current.step_settings, &configuration.step_settings)
                        || current.model_info_overrides != configuration.model_info_overrides
                    {
                        rejection =
                            Some("thread settings changed during timer activation".to_string());
                        return false;
                    }
                    if let Some(next) = &next_settings
                        && let Err(error) = next
                            .revalidate(&current.step_settings_constraints(&current_environments))
                    {
                        rejection = Some(error.to_string());
                        return false;
                    }
                    true
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| rejection.unwrap_or_else(|| "timer activation rejected".to_string()))?;
        if let Some(task) = task
            && let Some(next_settings) = next_settings
        {
            task.turn_context
                .next_step_settings
                .store(Arc::new(next_settings));
        }
        drop(active);
        // Preserve the current turn's goal-continuation ownership. Standalone
        // thread_settings::update would clear it as an explicit user override.
        thread_settings::emit_applied(self, super::new_submission_id(), commit.snapshot).await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "task_timer_tests.rs"]
mod tests;
