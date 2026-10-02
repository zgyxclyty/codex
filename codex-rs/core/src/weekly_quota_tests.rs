use super::*;
use codex_http_client::OutboundProxyPolicy;
use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const NOW: i64 = 1_800_000_000;

fn usage(used_percent: f64) -> RateLimitsWithResetCredits {
    RateLimitsWithResetCredits {
        rate_limits: vec![RateLimitSnapshot {
            limit_id: Some("codex".into()),
            limit_name: None,
            normal_model_slug: None,
            primary: Some(RateLimitWindow {
                used_percent: 20.0,
                window_minutes: Some(300),
                resets_at: Some(NOW + 3600),
            }),
            secondary: Some(RateLimitWindow {
                used_percent,
                window_minutes: Some(WEEK_MINUTES),
                resets_at: Some(NOW + 604800),
            }),
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: true,
                balance: Some("10000".into()),
            }),
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }],
        ordinary_usage_allowed: Some(true),
        rate_limit_reset_credits: None,
        account_id: None,
        user_id: None,
        rate_limit_upsell: None,
    }
}

fn check(usage: &RateLimitsWithResetCredits, percent: u8) -> Result<()> {
    WeeklyQuotaReserve::new(percent, String::new()).check_usage(usage, Some("test-model"), NOW)
}

#[test]
fn allows_above_reserve_and_stops_at_or_below_it_even_with_unlimited_credits() {
    for (used, allowed) in [
        (0.0, true),
        (89.9, true),
        (90.0, false),
        (90.1, false),
        (100.0, false),
        (110.0, false),
    ] {
        let result = check(&usage(used), 10);
        assert_eq!(result.is_ok(), allowed, "used={used}");
        if let Err(error) = result {
            assert!(error.to_string().contains("Weekly quota guard stopped"));
            assert!(
                error.retry_delay(1).is_none(),
                "guard failures must be terminal"
            );
        }
    }
}

#[test]
fn zero_reserve_still_blocks_exhausted_included_usage() {
    assert!(check(&usage(99.9), 0).is_ok());
    assert!(check(&usage(100.0), 0).is_err());
    assert!(check(&usage(0.0), 100).is_err());
}

#[test]
fn blocks_exhausted_short_window_and_backend_denial() {
    let mut snapshot = usage(10.0);
    snapshot.rate_limits[0]
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 100.0;
    assert!(check(&snapshot, 10).is_err());
    let mut snapshot = usage(10.0);
    snapshot.ordinary_usage_allowed = Some(false);
    assert!(check(&snapshot, 10).is_err());
}

#[test]
fn detects_weekly_window_in_either_position() {
    let mut snapshot = usage(95.0);
    let bucket = &mut snapshot.rate_limits[0];
    std::mem::swap(&mut bucket.primary, &mut bucket.secondary);
    assert!(check(&snapshot, 10).is_err());
    snapshot.rate_limits[0]
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 10.0;
    assert!(check(&snapshot, 10).is_ok());
}

#[test]
fn rejects_missing_invalid_and_expired_weekly_data() {
    let mut snapshot = usage(10.0);
    snapshot.rate_limits.clear();
    assert!(check(&snapshot, 10).is_err());
    let mut snapshot = usage(10.0);
    snapshot.rate_limits[0]
        .secondary
        .as_mut()
        .unwrap()
        .window_minutes = None;
    assert!(check(&snapshot, 10).is_err());
    for value in [f64::NAN, f64::INFINITY, -1.0] {
        assert!(check(&usage(value), 10).is_err());
    }
    let mut snapshot = usage(10.0);
    snapshot.rate_limits[0]
        .secondary
        .as_mut()
        .unwrap()
        .resets_at = Some(NOW);
    assert!(check(&snapshot, 10).is_err());
}

#[test]
fn missing_optional_reset_and_allowed_fields_do_not_disable_valid_percentage_guard() {
    let mut snapshot = usage(10.0);
    snapshot.ordinary_usage_allowed = None;
    snapshot.rate_limits[0]
        .secondary
        .as_mut()
        .unwrap()
        .resets_at = None;
    assert!(check(&snapshot, 10).is_ok());
    snapshot.rate_limits[0]
        .secondary
        .as_mut()
        .unwrap()
        .used_percent = 95.0;
    assert!(check(&snapshot, 10).is_err());
}

#[test]
fn checks_selected_model_quota_and_ignores_unrelated_buckets() {
    let mut snapshot = usage(10.0);
    let mut extra = snapshot.rate_limits[0].clone();
    extra.limit_id = Some("special-quota".into());
    extra.normal_model_slug = Some("other-model".into());
    extra.secondary.as_mut().unwrap().used_percent = 100.0;
    snapshot.rate_limits.push(extra);
    assert!(check(&snapshot, 10).is_ok());
    snapshot.rate_limits[1].normal_model_slug = Some("test-model".into());
    assert!(check(&snapshot, 10).is_err());
}

#[test]
fn accepts_shared_bucket_without_limit_id() {
    let mut snapshot = usage(10.0);
    snapshot.rate_limits[0].limit_id = None;
    assert!(check(&snapshot, 10).is_ok());
}

fn usage_body(used: u32) -> serde_json::Value {
    serde_json::json!({
        "plan_type": "plus",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {"used_percent": 20, "limit_window_seconds": 18000,
                "reset_after_seconds": 3600, "reset_at": 2000000000},
            "secondary_window": {"used_percent": used, "limit_window_seconds": 604800,
                "reset_after_seconds": 604800, "reset_at": 2000000000}
        },
        "credits": {"has_credits": true, "unlimited": true, "balance": "10000"}
    })
}

#[tokio::test]
async fn refreshes_usage_for_every_request_and_recovers_after_reset() {
    let server = MockServer::start().await;
    let guard = WeeklyQuotaReserve::new(10, server.uri());
    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    for (used, allowed) in [(89, true), (90, false), (99, false), (0, true)] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage_body(used)))
            .expect(1)
            .mount(&server)
            .await;
        let result = guard
            .check(
                Some(&auth),
                Some("test-model"),
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            )
            .await;
        assert_eq!(result.is_ok(), allowed);
        server.verify().await;
    }
}

#[tokio::test]
async fn lookup_failure_and_malformed_data_fail_closed() {
    let server = MockServer::start().await;
    let guard = WeeklyQuotaReserve::new(10, server.uri());
    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    for response in [
        ResponseTemplate::new(503),
        ResponseTemplate::new(200).set_body_string("invalid JSON"),
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"plan_type": "plus"})),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(response)
            .mount(&server)
            .await;
        let error = guard
            .check(
                Some(&auth),
                None,
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            )
            .await
            .unwrap_err();
        assert!(error.retry_delay(1).is_none());
    }
}

#[tokio::test]
async fn api_key_and_unauthenticated_providers_skip_usage_lookup() {
    let server = MockServer::start().await;
    let guard = WeeklyQuotaReserve::new(10, server.uri());
    let api_key_auth = CodexAuth::from_api_key("test-key");
    for auth in [None, Some(&api_key_auth)] {
        guard
            .check(
                auth,
                None,
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            )
            .await
            .unwrap();
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn usage_lookup_timeout_is_terminal() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(usage_body(0))
                .set_delay(USAGE_TIMEOUT + Duration::from_secs(1)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let guard = WeeklyQuotaReserve::new(10, server.uri());
    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let error = guard
        .check(
            Some(&auth),
            None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(error.retry_delay(1).is_none());
}
