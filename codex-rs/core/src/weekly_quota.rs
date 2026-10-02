//! Read-only admission checks for included ChatGPT usage.

use codex_backend_client::Client;
use codex_backend_client::RateLimitsWithResetCredits;
use codex_http_client::HttpClientFactory;
use codex_login::CodexAuth;
use codex_model_provider::SharedModelProvider;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use std::time::Duration;

const WEEK_MINUTES: i64 = 7 * 24 * 60;
const USAGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct WeeklyQuotaReserve {
    percent: u8,
    chatgpt_base_url: String,
}

impl WeeklyQuotaReserve {
    /// Create a read-only quota guard. Percentages must be between 0 and 100.
    pub fn new(percent: u8, chatgpt_base_url: String) -> Self {
        Self {
            percent,
            chatgpt_base_url,
        }
    }

    /// Check included usage using the authentication pinned to a model request.
    /// API-key authentication does not have a ChatGPT weekly quota.
    pub async fn check(
        &self,
        auth: Option<&CodexAuth>,
        model: Option<&str>,
        http_client_factory: HttpClientFactory,
    ) -> Result<()> {
        let Some(auth) = auth.filter(|auth| auth.uses_codex_backend()) else {
            return Ok(());
        };
        if self.percent > 100 {
            return Err(stopped(
                "weekly_quota_reserve_percent must be between 0 and 100",
            ));
        }

        // Query on every admission: another thread, CLI process, or ChatGPT
        // session can consume the same allowance. Never infer headroom from
        // purchased credits or redeem a reset credit to bypass this guard.
        let client = Client::from_auth(&self.chatgpt_base_url, auth, http_client_factory);
        let usage =
            tokio::time::timeout(USAGE_TIMEOUT, client.get_rate_limits_with_reset_credits())
                .await
                .map_err(|_| {
                    stopped("usage lookup timed out; included usage could not be verified")
                })?
                .map_err(|_| {
                    stopped("usage lookup failed; included usage could not be verified")
                })?;
        self.check_usage(&usage, model, chrono::Utc::now().timestamp())
    }

    /// Check standalone provider-backed tools using the same admission policy.
    pub async fn check_provider(
        &self,
        provider: &SharedModelProvider,
        model: Option<&str>,
        http_client_factory: HttpClientFactory,
    ) -> Result<()> {
        let info = provider.info();
        if !info.is_openai()
            || !info.requires_openai_auth
            || info.env_key.is_some()
            || info.experimental_bearer_token.is_some()
            || info.auth.is_some()
            || info.aws.is_some()
        {
            return Ok(());
        }
        let auth = provider.auth().await;
        self.check(auth.as_ref(), model, http_client_factory).await
    }

    fn check_usage(
        &self,
        usage: &RateLimitsWithResetCredits,
        model: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if usage.ordinary_usage_allowed == Some(false) {
            return Err(stopped(
                "included usage is unavailable; further requests may use credits",
            ));
        }

        let mut weekly_seen = false;
        for snapshot in &usage.rate_limits {
            let shared = snapshot.limit_id.as_deref().is_none_or(|id| id == "codex");
            let selected_model = model.is_some_and(|model| {
                snapshot.normal_model_slug.as_deref() == Some(model)
                    || snapshot.limit_id.as_deref() == Some(model)
            });
            if !shared && !selected_model {
                continue;
            }
            for window in snapshot.primary.iter().chain(snapshot.secondary.iter()) {
                if !window.used_percent.is_finite() || window.used_percent < 0.0 {
                    return Err(stopped(
                        "usage lookup returned an invalid remaining percentage",
                    ));
                }
                if window.resets_at.is_some_and(|reset| reset <= now) {
                    return Err(stopped(
                        "usage lookup returned an expired window; retry to refresh usage",
                    ));
                }
                if window.used_percent >= 100.0 {
                    return Err(stopped(
                        "an included usage window is exhausted; further requests may use credits",
                    ));
                }
                if window.window_minutes == Some(WEEK_MINUTES) {
                    weekly_seen = true;
                    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
                    if remaining <= f64::from(self.percent) {
                        let reset = window.resets_at.map_or_else(
                            || "reset time unavailable".to_string(),
                            |reset| {
                                chrono::DateTime::from_timestamp(reset, 0).map_or_else(
                                    || format!("resets at Unix timestamp {reset}"),
                                    |reset| {
                                        format!("resets at {} UTC", reset.format("%Y-%m-%d %H:%M"))
                                    },
                                )
                            },
                        );
                        return Err(stopped(format!(
                            "weekly remaining usage {remaining:.1}% is at or below the {}% reserve ({reset})",
                            self.percent,
                        )));
                    }
                }
            }
        }
        if !weekly_seen {
            return Err(stopped(
                "usage lookup did not report a weekly window; weekly remaining usage could not be verified",
            ));
        }
        Ok(())
    }
}

fn stopped(reason: impl std::fmt::Display) -> CodexErr {
    // A terminal error stops stream retries and goal continuation. Keep this
    // distinct from the server's usage-limit UI, which offers paid credits.
    CodexErr::InvalidRequest(format!(
        "Weekly quota guard stopped this request: {reason}. Wait for usage to reset or change weekly_quota_reserve_percent in config.toml."
    ))
}

#[cfg(test)]
#[path = "weekly_quota_tests.rs"]
mod tests;
