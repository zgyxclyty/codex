use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageRequestError;
use codex_api::ImageResponse;
use codex_api::ImagesClient;
use codex_api::ReqwestTransport;
use codex_api::map_api_error;
use codex_core::WeeklyQuotaReserve;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_login::default_client::add_originator_header;
use codex_login::default_client::create_transport_for_routes_async;
use codex_model_provider::SharedModelProvider;
use codex_protocol::error::CodexErr;
use http::HeaderMap;
use http::HeaderValue;

const X_CODEX_IMAGE_TURN_ID_HEADER: &str = "x-codex-image-turn-id";

pub(crate) struct ImageBackendError {
    message: String,
    codex_error: CodexErr,
    imagegen_request_id: Option<String>,
}

impl ImageBackendError {
    fn from_codex_error(codex_error: CodexErr) -> Self {
        Self {
            message: codex_error.to_string(),
            codex_error,
            imagegen_request_id: None,
        }
    }
    fn from_image_request(error: ImageRequestError) -> Self {
        let (error, imagegen_request_id) = error.into_parts();
        let message = error.to_string();
        Self {
            message,
            codex_error: map_api_error(error),
            imagegen_request_id,
        }
    }

    fn from_message(message: String) -> Self {
        Self {
            codex_error: CodexErr::Stream(message.clone()),
            message,
            imagegen_request_id: None,
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn codex_error(&self) -> &CodexErr {
        &self.codex_error
    }

    pub(crate) fn imagegen_request_id(&self) -> Option<&str> {
        self.imagegen_request_id.as_deref()
    }
}

#[derive(Clone)]
pub(crate) struct CodexImagesBackend {
    provider: SharedModelProvider,
    http_client_factory: HttpClientFactory,
    originator: Option<String>,
    weekly_quota_reserve: Option<WeeklyQuotaReserve>,
}

impl CodexImagesBackend {
    /// Creates a backend that sends image requests through the active model provider.
    pub(crate) fn new(
        provider: SharedModelProvider,
        http_client_factory: HttpClientFactory,
        originator: Option<String>,
    ) -> Self {
        Self {
            provider,
            http_client_factory,
            originator,
            weekly_quota_reserve: None,
        }
    }

    pub(crate) fn with_weekly_quota_reserve(mut self, guard: Option<WeeklyQuotaReserve>) -> Self {
        self.weekly_quota_reserve = guard;
        self
    }

    /// Resolves the provider and auth required for the current image API request.
    async fn client(&self) -> Result<ImagesClient<ReqwestTransport>, ImageBackendError> {
        if let Some(guard) = &self.weekly_quota_reserve {
            guard
                .check_provider(&self.provider, None, self.http_client_factory.clone())
                .await
                .map_err(ImageBackendError::from_codex_error)?;
        }
        let provider = self
            .provider
            .api_provider()
            .await
            .map_err(|err| ImageBackendError::from_message(err.to_string()))?;
        let auth = self
            .provider
            .api_auth()
            .await
            .map_err(|err| ImageBackendError::from_message(err.to_string()))?;
        let transport = create_transport_for_routes_async(
            self.http_client_factory.clone(),
            ClientRouteClass::Api,
        )
        .await
        .map_err(|err| ImageBackendError::from_message(err.to_string()))?;
        Ok(ImagesClient::new(transport, provider, auth))
    }

    /// Sends a standalone image generation request through the configured Images client.
    pub(crate) async fn generate(
        &self,
        request: ImageGenerationRequest,
        turn_id: &str,
    ) -> Result<(ImageResponse, Option<String>), ImageBackendError> {
        self.client()
            .await?
            .generate(
                &request,
                image_request_headers(self.originator.as_deref(), turn_id),
            )
            .await
            .map_err(ImageBackendError::from_image_request)
    }

    /// Sends a standalone image edit request through the configured Images client.
    pub(crate) async fn edit(
        &self,
        request: ImageEditRequest,
        turn_id: &str,
    ) -> Result<(ImageResponse, Option<String>), ImageBackendError> {
        self.client()
            .await?
            .edit(
                &request,
                image_request_headers(self.originator.as_deref(), turn_id),
            )
            .await
            .map_err(ImageBackendError::from_image_request)
    }
}

fn image_request_headers(originator: Option<&str>, turn_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(turn_id) = HeaderValue::from_str(turn_id) {
        headers.insert(X_CODEX_IMAGE_TURN_ID_HEADER, turn_id);
    }
    if let Some(originator) = originator {
        add_originator_header(&mut headers, originator);
    }
    headers
}

#[cfg(test)]
mod weekly_quota_tests {
    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
    use codex_model_provider::create_model_provider;
    use codex_model_provider_info::ModelProviderInfo;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn weekly_quota_blocks_image_generation_and_edits() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "plus",
                "rate_limit": {
                    "allowed": true, "limit_reached": false,
                    "secondary_window": {"used_percent": 90, "limit_window_seconds": 604800,
                        "reset_after_seconds": 604800, "reset_at": 2000000000}
                },
                "credits": {"has_credits": true, "unlimited": true, "balance": "10000"}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let backend = CodexImagesBackend::new(
            create_model_provider(
                ModelProviderInfo::create_openai_provider(Some(server.uri())),
                Some(AuthManager::from_auth_for_testing(
                    CodexAuth::create_dummy_chatgpt_auth_for_testing(),
                )),
            ),
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            None,
        )
        .with_weekly_quota_reserve(Some(WeeklyQuotaReserve::new(10, server.uri())));
        let generated = backend
            .generate(
                ImageGenerationRequest {
                    prompt: "test".into(),
                    background: None,
                    model: "gpt-image-2".into(),
                    n: None,
                    quality: None,
                    size: None,
                },
                "turn-1",
            )
            .await;
        let edited = backend
            .edit(
                ImageEditRequest {
                    images: Vec::new(),
                    prompt: "test".into(),
                    background: None,
                    model: "gpt-image-2".into(),
                    n: None,
                    quality: None,
                    size: None,
                },
                "turn-1",
            )
            .await;
        for result in [generated, edited] {
            let error = result.err().expect("guard must stop image requests");
            assert!(error.message().contains("Weekly quota guard stopped"));
            assert!(error.codex_error().retry_delay(1).is_none());
        }
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "only usage lookups should reach the server"
        );
    }
}
