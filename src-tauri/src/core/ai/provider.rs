use std::net::IpAddr;
use std::time::{Duration, Instant};

use reqwest::header::AUTHORIZATION;
use reqwest::{Client, Request, StatusCode, Url};
use serde_json::json;

use super::command_error;
use super::config::{provider_requires_api_key, validate_connection_config};
use super::types::{AiCommandError, AiConfigInput, AiErrorCode, AiErrorKind};

pub const MAX_RESPONSE_BYTES: usize = 1_048_576;
pub const MAX_ANALYSIS_OUTPUT_TOKENS: u32 = 2_048;
const CHAT_COMPLETIONS_PATH: &str = "chat/completions";

/// Once request sending begins, every outcome stays in this type so callers
/// can truthfully report that a potentially billable network attempt occurred.
pub enum ProviderAttempt {
    Response {
        status: StatusCode,
        body: Vec<u8>,
        latency_ms: i64,
    },
    Failed {
        code: AiErrorCode,
        message: String,
        latency_ms: i64,
    },
}

/// Analysis request outcome. `retryable` is decided from the status so the
/// runner never retries authentication or request-shape failures.
pub enum AnalysisAttempt {
    Response {
        status: StatusCode,
        body: Vec<u8>,
        retry_after_secs: Option<u64>,
        latency_ms: i64,
    },
    Failed {
        code: AiErrorCode,
        message: String,
        retryable: bool,
        latency_ms: i64,
    },
}

pub fn validate_base_url(provider: &str, raw_url: &str) -> Result<Url, AiCommandError> {
    let url = Url::parse(raw_url).map_err(|_| invalid_base_url())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(invalid_base_url());
    }

    if url.scheme() == "http" && (provider != "ollama" || !is_loopback(&url)) {
        return Err(invalid_base_url());
    }
    Ok(url)
}

pub async fn send_minimal_completion(
    config: &AiConfigInput,
    api_key: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<ProviderAttempt, AiCommandError> {
    validate_connection_config(config)?;
    let base_url = validate_base_url(&config.provider, &config.base_url)?;
    let key_required = provider_requires_api_key(&config.provider);
    if key_required
        && api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .is_none()
    {
        return Err(command_error(
            AiErrorKind::Configuration,
            AiErrorCode::KeyUnavailable,
            "An API key is required for this provider.",
            false,
        ));
    }

    let loopback_http = base_url.scheme() == "http";
    let client = build_client(config.timeout_seconds, loopback_http, proxy_url)?;
    let endpoint = base_url
        .join(CHAT_COMPLETIONS_PATH)
        .map_err(|_| invalid_base_url())?;
    let request = build_request(
        &client,
        endpoint,
        config,
        if base_url.scheme() == "https" && key_required {
            api_key
        } else {
            None
        },
    )?;

    let started = Instant::now();
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            let code = if error.is_timeout() {
                AiErrorCode::HttpTimeout
            } else {
                AiErrorCode::HttpRequest
            };
            let message = if error.is_timeout() {
                "The AI connection test timed out."
            } else {
                "The AI provider could not be reached."
            };
            return Ok(ProviderAttempt::Failed {
                code,
                message: message.into(),
                latency_ms: elapsed_millis(started),
            });
        }
    };

    read_bounded_response(response, started).await
}

/// Send the real analysis payload. It reuses the same client security (no
/// redirects, loopback-only plaintext, HTTPS-only bearer, no cookie) and the
/// same bounded streaming read as the connection test; only the body shape and
/// Retry-After extraction differ.
pub async fn send_analysis_completion(
    config: &AiConfigInput,
    api_key: Option<&str>,
    proxy_url: Option<&str>,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<AnalysisAttempt, AiCommandError> {
    validate_connection_config(config)?;
    let base_url = validate_base_url(&config.provider, &config.base_url)?;
    let key_required = provider_requires_api_key(&config.provider);
    if key_required
        && api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .is_none()
    {
        return Err(command_error(
            AiErrorKind::Configuration,
            AiErrorCode::KeyUnavailable,
            "An API key is required for this provider.",
            false,
        ));
    }

    let loopback_http = base_url.scheme() == "http";
    let client = build_client(config.timeout_seconds, loopback_http, proxy_url)?;
    let endpoint = base_url
        .join(CHAT_COMPLETIONS_PATH)
        .map_err(|_| invalid_base_url())?;
    let request = build_analysis_request(
        &client,
        endpoint,
        config,
        if base_url.scheme() == "https" && key_required {
            api_key
        } else {
            None
        },
        system_prompt,
        user_prompt,
    )?;

    let started = Instant::now();
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            let code = if error.is_timeout() {
                AiErrorCode::HttpTimeout
            } else {
                AiErrorCode::HttpRequest
            };
            let message = if error.is_timeout() {
                "The AI analysis request timed out."
            } else {
                "The AI provider could not be reached for analysis."
            };
            return Ok(AnalysisAttempt::Failed {
                code,
                message: message.into(),
                retryable: true,
                latency_ms: elapsed_millis(started),
            });
        }
    };

    // Retry-After must be captured before the response is consumed by the
    // bounded reader; it is a plain numeric seconds value, never sensitive.
    let retry_after_secs = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    match read_bounded_response(response, started).await? {
        ProviderAttempt::Response {
            status,
            body,
            latency_ms,
        } => Ok(AnalysisAttempt::Response {
            status,
            body,
            retry_after_secs,
            latency_ms,
        }),
        ProviderAttempt::Failed {
            code,
            message,
            latency_ms,
        } => Ok(AnalysisAttempt::Failed {
            code,
            message,
            retryable: code == AiErrorCode::HttpTimeout || code == AiErrorCode::HttpRequest,
            latency_ms,
        }),
    }
}

fn build_client(
    timeout_seconds: u32,
    loopback_http: bool,
    proxy_url: Option<&str>,
) -> Result<Client, AiCommandError> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(u64::from(timeout_seconds)));

    if loopback_http {
        // Explicitly disable both configured and environment proxies so local
        // plaintext Ollama traffic cannot escape the loopback interface.
        builder = builder.no_proxy();
    } else if let Some(proxy_url) = proxy_url.filter(|value| !value.trim().is_empty()) {
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| {
            command_error(
                AiErrorKind::Configuration,
                AiErrorCode::InvalidConfig,
                "The configured proxy URL is invalid.",
                false,
            )
        })?;
        builder = builder.proxy(proxy);
    }

    builder.build().map_err(|_| {
        command_error(
            AiErrorKind::Provider,
            AiErrorCode::HttpRequest,
            "Unable to create the AI provider client.",
            false,
        )
    })
}

fn build_request(
    client: &Client,
    endpoint: Url,
    config: &AiConfigInput,
    api_key: Option<&str>,
) -> Result<Request, AiCommandError> {
    let mut request = client.post(endpoint).json(&json!({
        "model": config.model,
        "messages": [{ "role": "user", "content": "Reply with OK." }],
        "max_tokens": 1,
        "temperature": 0
    }));
    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    // Never set Cookie, and never expose RequestBuilder/HeaderMap in an error;
    // either could contain authentication material at this point.
    request.build().map_err(|_| {
        command_error(
            AiErrorKind::Provider,
            AiErrorCode::HttpRequest,
            "Unable to create the AI provider request.",
            false,
        )
    })
}

fn build_analysis_request(
    client: &Client,
    endpoint: Url,
    config: &AiConfigInput,
    api_key: Option<&str>,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<Request, AiCommandError> {
    let mut request = client.post(endpoint).json(&json!({
        "model": config.model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ],
        "max_tokens": MAX_ANALYSIS_OUTPUT_TOKENS,
        "temperature": 0
    }));
    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    // Mirror build_request: never attach Cookie and never expose the builder
    // in an error because either could carry authentication material.
    request.build().map_err(|_| {
        command_error(
            AiErrorKind::Provider,
            AiErrorCode::HttpRequest,
            "Unable to create the AI analysis request.",
            false,
        )
    })
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    started: Instant,
) -> Result<ProviderAttempt, AiCommandError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Ok(response_too_large_attempt(started));
    }

    let initial_capacity = response
        .content_length()
        .unwrap_or(0)
        .min(MAX_RESPONSE_BYTES as u64) as usize;
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                let code = if error.is_timeout() {
                    AiErrorCode::HttpTimeout
                } else {
                    AiErrorCode::HttpRequest
                };
                return Ok(ProviderAttempt::Failed {
                    code,
                    message: "The AI provider response could not be read.".into(),
                    latency_ms: elapsed_millis(started),
                });
            }
        };
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
            // Abort before extending the buffer so chunked responses can never
            // allocate beyond the frozen one-megabyte response boundary.
            return Ok(response_too_large_attempt(started));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(ProviderAttempt::Response {
        status,
        body,
        latency_ms: elapsed_millis(started),
    })
}

pub fn connection_message(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200..=299 => "AI provider connection succeeded.",
        401 | 403 => "AI provider authentication failed.",
        429 => "The AI provider rate limit was reached.",
        500..=599 => "The AI provider is temporarily unavailable.",
        _ => "The AI provider rejected the connection test.",
    }
}

fn response_too_large_attempt(started: Instant) -> ProviderAttempt {
    ProviderAttempt::Failed {
        code: AiErrorCode::ResponseTooLarge,
        message: "The AI provider response exceeded the one-megabyte limit.".into(),
        latency_ms: elapsed_millis(started),
    }
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // reqwest's URL host representation retains IPv6 brackets, while
    // IpAddr parsing expects the address alone.
    let address_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || address_host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn elapsed_millis(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn invalid_base_url() -> AiCommandError {
    command_error(
        AiErrorKind::Security,
        AiErrorCode::InvalidBaseUrl,
        "The AI provider base URL is not allowed.",
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{AUTHORIZATION, COOKIE};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn config(provider: &str, base_url: String) -> AiConfigInput {
        AiConfigInput {
            provider: provider.into(),
            base_url,
            model: "test-model".into(),
            output_language: "en".into(),
            timeout_seconds: 2,
            concurrency: 1,
            log_retention_days: 30,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
        }
    }

    async fn serve_once(
        status: u16,
        extra_headers: &str,
        body: Vec<u8>,
    ) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        let headers = extra_headers.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
            let reason = if status == 200 { "OK" } else { "Test" };
            let response_head = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                body.len()
            );
            socket.write_all(response_head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        (format!("http://{address}/v1/"), receiver)
    }

    async fn serve_chunked_oversize() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            if socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .is_err()
            {
                return;
            }
            // No Content-Length is sent, forcing the production chunk loop to
            // enforce the limit rather than relying on response metadata.
            let chunk = vec![b'x'; 16_384];
            for _ in 0..=MAX_RESPONSE_BYTES / chunk.len() {
                if socket
                    .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                    .await
                    .is_err()
                    || socket.write_all(&chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        format!("http://{address}/v1/")
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_base_urls() {
        for url in [
            "http://example.com/v1/",
            "http://127.0.0.1:11434/v1/?q=1",
            "https://user@example.com/v1/",
            "https://example.com/v1/#fragment",
            "https://example.com/v1",
            "file:///tmp/v1/",
        ] {
            assert_eq!(
                validate_base_url("ollama", url).unwrap_err().code,
                AiErrorCode::InvalidBaseUrl
            );
        }
        assert!(validate_base_url("ollama", "http://127.99.1.2:11434/v1/").is_ok());
        assert!(validate_base_url("ollama", "http://[::1]:11434/v1/").is_ok());
        assert!(validate_base_url("custom", "http://127.0.0.1:8080/v1/").is_err());
    }

    #[tokio::test]
    async fn loopback_http_uses_exact_path_without_auth_or_proxy() {
        let (base_url, request) = serve_once(200, "", b"{}".to_vec()).await;
        let attempt = send_minimal_completion(
            &config("ollama", base_url),
            Some("placeholder-credential"),
            Some("http://127.0.0.1:9"),
        )
        .await
        .unwrap();
        assert!(matches!(attempt, ProviderAttempt::Response { status, .. } if status == 200));
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /v1/chat/completions http/1.1\r\n"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("cookie:"));
    }

    #[test]
    fn https_request_carries_bearer_but_never_cookie_without_sending() {
        let config = config("openai", "https://example.invalid/v1/".into());
        let base = validate_base_url(&config.provider, &config.base_url).unwrap();
        let client = build_client(2, false, None).unwrap();
        let request = build_request(
            &client,
            base.join(CHAT_COMPLETIONS_PATH).unwrap(),
            &config,
            Some("placeholder-credential"),
        )
        .unwrap();
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer placeholder-credential"
        );
        assert!(request.headers().get(COOKIE).is_none());
    }

    #[test]
    fn analysis_request_uses_frozen_prompt_shape_and_budget() {
        let config = config("ollama", "http://127.0.0.1:11434/v1/".into());
        let base = validate_base_url(&config.provider, &config.base_url).unwrap();
        let client = build_client(2, true, None).unwrap();
        let request = build_analysis_request(
            &client,
            base.join(CHAT_COMPLETIONS_PATH).unwrap(),
            &config,
            None,
            "system-instructions",
            "untrusted-document",
        )
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "system-instructions");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "untrusted-document");
        assert!(request.headers().get(COOKIE).is_none());
    }

    #[tokio::test]
    async fn redirects_are_returned_without_following() {
        let (base_url, request) = serve_once(
            302,
            "Location: http://127.0.0.1:9/redirected\r\n",
            Vec::new(),
        )
        .await;
        let attempt = send_minimal_completion(&config("ollama", base_url), None, None)
            .await
            .unwrap();
        assert!(matches!(attempt, ProviderAttempt::Response { status, .. } if status == 302));
        assert!(request
            .await
            .unwrap()
            .starts_with("POST /v1/chat/completions "));
    }

    #[tokio::test]
    async fn response_over_one_megabyte_is_stopped() {
        let base_url = serve_chunked_oversize().await;
        let attempt = send_minimal_completion(&config("ollama", base_url), None, None)
            .await
            .unwrap();
        assert!(matches!(
            attempt,
            ProviderAttempt::Failed {
                code: AiErrorCode::ResponseTooLarge,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn provider_http_failures_preserve_status_for_connection_results() {
        for status in [401, 403, 429, 500] {
            let (base_url, _request) = serve_once(status, "", b"ignored".to_vec()).await;
            let attempt = send_minimal_completion(&config("ollama", base_url), None, None)
                .await
                .unwrap();
            assert!(
                matches!(attempt, ProviderAttempt::Response { status: actual, .. } if actual.as_u16() == status)
            );
            assert_ne!(
                connection_message(StatusCode::from_u16(status).unwrap()),
                ""
            );
        }
    }
}
