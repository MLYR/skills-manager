use crate::core::skill_store::SkillStore;
use serde_json::Value;

use super::command_error;
use super::provider::validate_base_url;
use super::types::{
    AiCommandError, AiConfigDto, AiConfigInput, AiErrorCode, AiErrorKind, AiProviderPresetDto,
};

pub const AI_CONFIG_SETTING_KEY: &str = "ai_analysis_config_v1";
pub const DEFAULT_LOG_RETENTION_DAYS: u16 = 30;
const MAX_API_KEY_BYTES: usize = 16_384;
const API_KEY_FIELD: &str = "api_key";

/// Only a genuinely absent setting receives defaults; malformed or obsolete
/// JSON must stay visible as invalid_config instead of silently changing spend.
pub fn load_config(store: &SkillStore) -> Result<AiConfigInput, AiCommandError> {
    load_config_and_api_key(store).map(|(config, _)| config)
}

pub fn load_api_key(store: &SkillStore) -> Result<Option<String>, AiCommandError> {
    load_config_and_api_key(store).map(|(_, api_key)| api_key)
}

pub fn load_config_and_api_key(
    store: &SkillStore,
) -> Result<(AiConfigInput, Option<String>), AiCommandError> {
    // 配置和 Key 共用同一 settings 行，读取时一次解析，避免测试与 Runner 使用不同来源。
    let raw = store.get_setting(AI_CONFIG_SETTING_KEY).map_err(|_| {
        command_error(
            AiErrorKind::Storage,
            AiErrorCode::Db,
            "Unable to read the AI configuration.",
            true,
        )
    })?;

    match raw {
        Some(raw) => parse_persisted_config_with_key(&raw),
        None => Ok((default_config(), None)),
    }
}

pub fn save_config(store: &SkillStore, input: &AiConfigInput) -> Result<(), AiCommandError> {
    save_config_with_api_key(store, input, None)
}

pub fn save_config_with_api_key(
    store: &SkillStore,
    input: &AiConfigInput,
    api_key: Option<&str>,
) -> Result<(), AiCommandError> {
    validate_config(input)?;
    let persisted_key = match api_key {
        Some(api_key) => normalize_api_key(api_key)?,
        None => load_api_key(store)?,
    };
    // Key 只进入本地配置 JSON；settings 层会按现有规则加密整行。
    let mut persisted_config = input.clone();
    // 新配置不再保存旧价格字段，避免历史单价继续影响后续界面或批次。
    persisted_config.input_price_micros_per_million = None;
    persisted_config.output_price_micros_per_million = None;
    let mut value = serde_json::to_value(&persisted_config).map_err(|_| {
        command_error(
            AiErrorKind::Internal,
            AiErrorCode::Internal,
            "Unable to serialize the AI configuration.",
            false,
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        command_error(
            AiErrorKind::Internal,
            AiErrorCode::Internal,
            "Unable to serialize the AI configuration.",
            false,
        )
    })?;
    if let Some(api_key) = persisted_key {
        object.insert(API_KEY_FIELD.into(), Value::String(api_key));
    } else {
        object.remove(API_KEY_FIELD);
    }
    let serialized = serde_json::to_string(&value).map_err(|_| {
        command_error(
            AiErrorKind::Internal,
            AiErrorCode::Internal,
            "Unable to serialize the AI configuration.",
            false,
        )
    })?;
    // A single settings row is the atomic unit, preventing partially updated
    // provider and pricing fields from being observed by a concurrent reader.
    store
        .set_setting(AI_CONFIG_SETTING_KEY, &serialized)
        .map_err(|_| {
            command_error(
                AiErrorKind::Storage,
                AiErrorCode::Db,
                "Unable to save the AI configuration.",
                true,
            )
        })
}

pub fn parse_persisted_config(raw: &str) -> Result<AiConfigInput, AiCommandError> {
    parse_persisted_config_with_key(raw).map(|(config, _)| config)
}

pub fn parse_persisted_config_with_key(
    raw: &str,
) -> Result<(AiConfigInput, Option<String>), AiCommandError> {
    let mut value: Value = serde_json::from_str(raw).map_err(|_| invalid_config())?;
    let api_key = match value.get(API_KEY_FIELD) {
        None | Some(Value::Null) => None,
        Some(Value::String(api_key)) => normalize_api_key(api_key)?,
        Some(_) => return Err(invalid_config()),
    };
    let object = value.as_object_mut().ok_or_else(invalid_config)?;
    object.remove(API_KEY_FIELD);
    let config: AiConfigInput =
        serde_json::from_value(Value::Object(object.clone())).map_err(|_| invalid_config())?;
    validate_config(&config)?;
    Ok((config, api_key))
}

pub fn validate_config(config: &AiConfigInput) -> Result<(), AiCommandError> {
    if !matches!(
        config.provider.as_str(),
        "openai" | "deepseek" | "openrouter" | "ollama" | "custom"
    ) {
        return Err(invalid_config());
    }
    if config.model.trim().is_empty()
        || !(1..=300).contains(&config.timeout_seconds)
        || !(1..=5).contains(&config.concurrency)
        || !(1..=3650).contains(&config.log_retention_days)
        || !matches!(
            config.output_language.as_str(),
            "auto" | "zh" | "zh-TW" | "en"
        )
    {
        return Err(invalid_config());
    }
    validate_base_url(&config.provider, &config.base_url)?;
    Ok(())
}

/// Connection tests distinguish a fresh, intentionally empty default from a
/// corrupt persisted configuration so the UI can offer setup as the remedy.
pub fn validate_connection_config(config: &AiConfigInput) -> Result<(), AiCommandError> {
    if config.base_url.trim().is_empty() || config.model.trim().is_empty() {
        return Err(command_error(
            AiErrorKind::Configuration,
            AiErrorCode::NotConfigured,
            "AI service configuration is incomplete.",
            false,
        ));
    }
    validate_config(config)
}

pub fn validate_model_list_config(config: &AiConfigInput) -> Result<(), AiCommandError> {
    // 获取模型列表不依赖已选模型；用临时值复用其余配置和 URL 安全校验，避免自定义服务商陷入先填模型才能获取模型的循环。
    let mut validation_config = config.clone();
    if validation_config.model.trim().is_empty() {
        validation_config.model = "model-list-placeholder".into();
    }
    validate_connection_config(&validation_config)
}

pub fn to_dto(config: AiConfigInput, api_key: Option<String>) -> AiConfigDto {
    let has_api_key = api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let is_configured = validate_connection_config(&config).is_ok()
        && (!provider_requires_api_key(&config.provider) || has_api_key);
    AiConfigDto {
        provider: config.provider,
        base_url: config.base_url,
        model: config.model,
        output_language: config.output_language,
        timeout_seconds: config.timeout_seconds,
        concurrency: config.concurrency,
        log_retention_days: config.log_retention_days,
        api_key,
        has_api_key,
        is_configured,
    }
}

pub fn provider_requires_api_key(provider: &str) -> bool {
    provider != "ollama"
}

pub fn provider_presets() -> Vec<AiProviderPresetDto> {
    vec![
        preset(
            "openai",
            "OpenAI",
            "https://api.openai.com/v1/",
            Some("gpt-4o-mini"),
            true,
        ),
        preset(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com/v1/",
            Some("deepseek-chat"),
            true,
        ),
        preset(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1/",
            None,
            true,
        ),
        preset(
            "ollama",
            "Ollama",
            "http://127.0.0.1:11434/v1/",
            None,
            false,
        ),
        preset("custom", "Custom", "", None, true),
    ]
}

/// Retention falls back only for this privacy-preserving cleanup path; normal
/// configuration reads still surface invalid_config to the caller.
pub fn retention_days_for_cleanup(store: &SkillStore) -> Result<u16, AiCommandError> {
    let raw = store.get_setting(AI_CONFIG_SETTING_KEY).map_err(|_| {
        command_error(
            AiErrorKind::Storage,
            AiErrorCode::Db,
            "Unable to read the AI log retention setting.",
            true,
        )
    })?;
    let Some(raw) = raw else {
        return Ok(DEFAULT_LOG_RETENTION_DAYS);
    };
    match parse_persisted_config(&raw) {
        Ok(config) => Ok(config.log_retention_days),
        Err(_) => {
            // Never include the corrupt JSON: it is untrusted and could itself
            // contain a credential copied into the wrong setting.
            log::warn!(
                "AI configuration is invalid; using the 30-day log retention safety default"
            );
            Ok(DEFAULT_LOG_RETENTION_DAYS)
        }
    }
}

fn default_config() -> AiConfigInput {
    AiConfigInput {
        provider: "custom".into(),
        base_url: String::new(),
        model: String::new(),
        output_language: "auto".into(),
        timeout_seconds: 60,
        concurrency: 1,
        log_retention_days: DEFAULT_LOG_RETENTION_DAYS,
        input_price_micros_per_million: None,
        output_price_micros_per_million: None,
    }
}

fn normalize_api_key(api_key: &str) -> Result<Option<String>, AiCommandError> {
    if api_key.as_bytes().len() > MAX_API_KEY_BYTES {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidConfig,
            "The API key cannot exceed 16384 UTF-8 bytes.",
            false,
        ));
    }
    if api_key.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(api_key.to_string()))
    }
}

fn preset(
    id: &str,
    display_name: &str,
    base_url: &str,
    default_model: Option<&str>,
    api_key_required: bool,
) -> AiProviderPresetDto {
    AiProviderPresetDto {
        id: id.into(),
        display_name: display_name.into(),
        base_url: base_url.into(),
        default_model: default_model.map(str::to_string),
        api_key_required,
    }
}

fn invalid_config() -> AiCommandError {
    command_error(
        AiErrorKind::Configuration,
        AiErrorCode::InvalidConfig,
        "The AI configuration is invalid.",
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, SkillStore) {
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("config.db")).unwrap();
        (directory, store)
    }

    fn valid_config() -> AiConfigInput {
        AiConfigInput {
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1/".into(),
            model: "gpt-test".into(),
            output_language: "zh-TW".into(),
            timeout_seconds: 60,
            concurrency: 2,
            log_retention_days: 30,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
        }
    }

    #[test]
    fn absent_setting_alone_uses_defaults() {
        let (_directory, store) = store();
        let config = load_config(&store).unwrap();
        assert_eq!(config.provider, "custom");
        assert_eq!(config.timeout_seconds, 60);
        assert_eq!(config.log_retention_days, 30);
    }

    #[test]
    fn configuration_round_trips_as_one_json_setting() {
        let (_directory, store) = store();
        let config = valid_config();
        save_config(&store, &config).unwrap();
        assert_eq!(load_config(&store).unwrap(), config);
        assert!(store.get_setting(AI_CONFIG_SETTING_KEY).unwrap().is_some());
    }

    #[test]
    fn local_api_key_round_trips_and_blank_input_clears_it() {
        let (_directory, store) = store();
        let config = valid_config();
        save_config_with_api_key(&store, &config, Some("local-test-key")).unwrap();
        assert_eq!(
            load_api_key(&store).unwrap().as_deref(),
            Some("local-test-key")
        );

        save_config_with_api_key(&store, &config, Some("   ")).unwrap();
        assert_eq!(load_api_key(&store).unwrap(), None);
    }

    #[test]
    fn saving_config_drops_legacy_price_fields() {
        let (_directory, store) = store();
        let mut config = valid_config();
        config.input_price_micros_per_million = Some(10);
        config.output_price_micros_per_million = Some(20);
        save_config(&store, &config).unwrap();

        let saved = load_config(&store).unwrap();
        assert_eq!(saved.input_price_micros_per_million, None);
        assert_eq!(saved.output_price_micros_per_million, None);
        let raw = store.get_setting(AI_CONFIG_SETTING_KEY).unwrap().unwrap();
        assert!(raw.contains("provider"));
        assert!(!raw.contains("input_price_micros_per_million"));
        assert!(!raw.contains("output_price_micros_per_million"));
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let mut config = valid_config();
        config.concurrency = 0;
        assert_eq!(
            validate_config(&config).unwrap_err().code,
            AiErrorCode::InvalidConfig
        );
    }

    #[test]
    fn model_list_validation_allows_empty_model() {
        let mut config = valid_config();
        config.model.clear();
        assert!(validate_model_list_config(&config).is_ok());
    }

    #[test]
    fn corrupt_and_unknown_json_never_silently_default() {
        let (_directory, store) = store();
        store.set_setting(AI_CONFIG_SETTING_KEY, "{").unwrap();
        assert_eq!(
            load_config(&store).unwrap_err().code,
            AiErrorCode::InvalidConfig
        );

        let mut value = serde_json::to_value(valid_config()).unwrap();
        value["unexpected"] = serde_json::json!(true);
        store
            .set_setting(AI_CONFIG_SETTING_KEY, &value.to_string())
            .unwrap();
        assert_eq!(
            load_config(&store).unwrap_err().code,
            AiErrorCode::InvalidConfig
        );
    }

    #[test]
    fn provider_presets_match_frozen_urls_and_key_policy() {
        let presets = provider_presets();
        assert_eq!(presets[0].base_url, "https://api.openai.com/v1/");
        assert_eq!(presets[1].base_url, "https://api.deepseek.com/v1/");
        assert_eq!(presets[2].base_url, "https://openrouter.ai/api/v1/");
        assert_eq!(presets[3].base_url, "http://127.0.0.1:11434/v1/");
        assert!(!presets[3].api_key_required);
        assert!(presets[4].api_key_required);
    }
}
