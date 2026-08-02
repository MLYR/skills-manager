use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use tauri::State;

use crate::core::ai::command_error;
use crate::core::ai::config::{
    load_config, provider_presets, provider_requires_api_key, save_config, to_dto,
    validate_connection_config,
};
use crate::core::ai::provider::{connection_message, send_minimal_completion, ProviderAttempt};
use crate::core::ai::secret_store::SecretStore;
use crate::core::ai::types::{
    AiApiKeyStatusDto, AiCommandError, AiConfigDto, AiConfigInput, AiConnectionTestDto,
    AiConnectionTestInput, AiErrorCode, AiErrorKind, AiProviderPresetDto,
};
use crate::core::skill_store::SkillStore;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct SetAiApiKeyInput {
    api_key: String,
}

#[tauri::command]
pub fn get_ai_provider_presets() -> Result<Vec<AiProviderPresetDto>, AiCommandError> {
    Ok(provider_presets())
}

#[tauri::command]
pub async fn get_ai_config(
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiConfigDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let config = load_config(&store)?;
        let has_api_key = SecretStore::new()?.has()?;
        Ok(to_dto(config, has_api_key))
    })
    .await
}

#[tauri::command]
pub async fn save_ai_config(
    input: AiConfigInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiConfigDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        // Validate both configuration and Keyring availability before writing,
        // avoiding a successful save followed by a misleading command failure.
        crate::core::ai::config::validate_config(&input)?;
        let has_api_key = SecretStore::new()?.has()?;
        save_config(&store, &input)?;
        Ok(to_dto(input, has_api_key))
    })
    .await
}

#[tauri::command]
pub async fn get_ai_api_key_status() -> Result<AiApiKeyStatusDto, AiCommandError> {
    run_blocking(|| {
        Ok(AiApiKeyStatusDto {
            has_api_key: SecretStore::new()?.has()?,
        })
    })
    .await
}

#[tauri::command]
pub async fn set_ai_api_key(input: SetAiApiKeyInput) -> Result<AiApiKeyStatusDto, AiCommandError> {
    run_blocking(move || {
        SecretStore::new()?.set(&input.api_key)?;
        Ok(AiApiKeyStatusDto { has_api_key: true })
    })
    .await
}

#[tauri::command]
pub async fn delete_ai_api_key() -> Result<AiApiKeyStatusDto, AiCommandError> {
    run_blocking(|| {
        SecretStore::new()?.delete()?;
        Ok(AiApiKeyStatusDto { has_api_key: false })
    })
    .await
}

#[tauri::command]
pub async fn test_ai_connection(
    input: AiConnectionTestInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiConnectionTestDto, AiCommandError> {
    validate_connection_config(&input.config)?;
    let started = Instant::now();
    // An unconfirmed test ends immediately after pure configuration and URL
    // validation, before Keyring, settings, client, DNS, or network access.
    if !input.confirm_billable_request {
        return Ok(local_validation_success(&input.config, started));
    }

    let store = store.inner().clone();
    let config = input.config;

    let (config, api_key, proxy_url) = run_blocking(move || {
        // Construct SecretStore only inside the required-provider loader so a
        // confirmed Ollama test remains independent from Keyring availability.
        let api_key =
            resolve_connection_api_key(&config.provider, true, || SecretStore::new()?.load())?;

        // Required credentials (when any) and proxy settings are resolved in
        // the same blocking phase immediately before the network request.
        let proxy_url = store.get_setting("proxy_url").map_err(|_| {
            command_error(
                AiErrorKind::Storage,
                AiErrorCode::Db,
                "Unable to read the proxy configuration.",
                true,
            )
        })?;
        Ok((config, api_key, proxy_url))
    })
    .await?;

    execute_connection_test(config, true, api_key, proxy_url, started).await
}

async fn execute_connection_test(
    config: AiConfigInput,
    confirm_billable_request: bool,
    api_key: Option<String>,
    proxy_url: Option<String>,
    started: Instant,
) -> Result<AiConnectionTestDto, AiCommandError> {
    if !confirm_billable_request {
        // This branch occurs before proxy parsing, client construction, DNS,
        // or request creation, so an unchecked test can never incur a charge.
        return Ok(local_validation_success(&config, started));
    }

    match send_minimal_completion(&config, api_key.as_deref(), proxy_url.as_deref()).await? {
        ProviderAttempt::Response {
            status,
            body,
            latency_ms,
        } => {
            // Connection tests intentionally do not expose or log provider
            // response bodies; reading it only enforces the global size bound.
            drop(body);
            Ok(AiConnectionTestDto {
                success: status.is_success(),
                provider: config.provider,
                model: config.model,
                message: connection_message(status).into(),
                http_status: Some(i64::from(status.as_u16())),
                latency_ms,
                billable_request_sent: true,
            })
        }
        ProviderAttempt::Failed {
            message,
            latency_ms,
            ..
        } => Ok(AiConnectionTestDto {
            success: false,
            provider: config.provider,
            model: config.model,
            message,
            http_status: None,
            latency_ms,
            billable_request_sent: true,
        }),
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T, AiCommandError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AiCommandError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|_| {
            command_error(
                AiErrorKind::Internal,
                AiErrorCode::Internal,
                "An AI background operation stopped unexpectedly.",
                true,
            )
        })?
}

fn ensure_required_key(provider: &str, has_api_key: bool) -> Result<(), AiCommandError> {
    if provider_requires_api_key(provider) && !has_api_key {
        return Err(command_error(
            AiErrorKind::Configuration,
            AiErrorCode::KeyUnavailable,
            "An API key is required for this provider.",
            false,
        ));
    }
    Ok(())
}

fn resolve_connection_api_key<F>(
    provider: &str,
    confirm_billable_request: bool,
    load_required_key: F,
) -> Result<Option<String>, AiCommandError>
where
    F: FnOnce() -> Result<Option<String>, AiCommandError>,
{
    // Keep this lower-level gate defensive for future call sites as well: no
    // unconfirmed or keyless request may invoke the Keyring-backed loader.
    if !confirm_billable_request || !provider_requires_api_key(provider) {
        return Ok(None);
    }

    let api_key = load_required_key()?;
    ensure_required_key(provider, api_key.is_some())?;
    Ok(api_key)
}

fn local_validation_success(config: &AiConfigInput, started: Instant) -> AiConnectionTestDto {
    AiConnectionTestDto {
        success: true,
        provider: config.provider.clone(),
        model: config.model.clone(),
        message: "Configuration is valid. No billable request was sent.".into(),
        http_status: None,
        latency_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        billable_request_sent: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AiConfigInput {
        AiConfigInput {
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1/".into(),
            model: "local-test-model".into(),
            output_language: "en".into(),
            timeout_seconds: 60,
            concurrency: 1,
            log_retention_days: 30,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
        }
    }

    #[test]
    fn unchecked_cloud_connection_result_is_explicitly_non_billable() {
        let mut config = valid_config();
        config.provider = "openai".into();
        config.base_url = "https://api.openai.com/v1/".into();
        validate_connection_config(&config).unwrap();
        let result = local_validation_success(&config, Instant::now());
        assert!(result.success);
        assert!(!result.billable_request_sent);
        assert_eq!(result.http_status, None);
    }

    #[tokio::test]
    async fn unchecked_connection_never_touches_the_network() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = valid_config();
        config.provider = "openai".into();
        config.base_url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let result = execute_connection_test(config, false, None, None, Instant::now())
            .await
            .unwrap();
        assert!(!result.billable_request_sent);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn command_inputs_reject_unknown_or_misnested_fields() {
        assert!(
            serde_json::from_value::<SetAiApiKeyInput>(serde_json::json!({
                "api_key": "placeholder",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<AiConnectionTestInput>(serde_json::json!({
                "config": valid_config(),
                "confirm_billable_request": false,
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<AiConnectionTestInput>(serde_json::json!({
                "provider": "ollama",
                "confirm_billable_request": false
            }))
            .is_err()
        );
    }

    #[test]
    fn all_unconfirmed_providers_skip_the_keyring_loader() {
        for provider in ["openai", "deepseek", "openrouter", "ollama", "custom"] {
            let loader_called = std::cell::Cell::new(false);
            let api_key = resolve_connection_api_key(provider, false, || {
                loader_called.set(true);
                Ok(Some("must-not-be-read".into()))
            })
            .unwrap();

            assert_eq!(api_key, None);
            assert!(!loader_called.get(), "loader called for {provider}");
        }
    }

    #[test]
    fn confirmed_cloud_provider_still_requires_a_key() {
        assert_eq!(
            resolve_connection_api_key("custom", true, || Ok(None))
                .unwrap_err()
                .code,
            AiErrorCode::KeyUnavailable
        );
    }

    #[test]
    fn confirmed_ollama_connection_never_invokes_the_keyring_loader() {
        let loader_called = std::cell::Cell::new(false);
        let api_key = resolve_connection_api_key("ollama", true, || {
            loader_called.set(true);
            Ok(Some("must-not-be-read".into()))
        })
        .unwrap();

        assert_eq!(api_key, None);
        assert!(!loader_called.get());
    }

    #[test]
    fn confirmed_required_provider_loads_the_key_value() {
        let loader_called = std::cell::Cell::new(false);
        let real_key = resolve_connection_api_key("openai", true, || {
            loader_called.set(true);
            Ok(Some("test-key".into()))
        })
        .unwrap();
        assert!(loader_called.get());
        assert_eq!(real_key.as_deref(), Some("test-key"));
    }
}
