//! Preview registry and cost estimation.
//!
//! The preview registry exists only in process memory with a short TTL: no
//! confirmed content or non-key configuration snapshot is ever persisted
//! before the user confirms, and a restart forces a fresh preview.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::command_error;
use super::document::{CollectedDocument, DocumentOutcome};
use super::types::{
    AiAnalysisMode, AiCommandError, AiConfigInput, AiErrorCode, AiErrorKind, AiPreviewEligibility,
    AiPreviewItemDto, AiTargetRef,
};

pub const PREVIEW_TTL_MILLIS: i64 = 10 * 60 * 1_000;
const FIXED_PROMPT_TOKENS: i64 = 512;
const MAX_OUTPUT_TOKENS: i64 = 2_048;

/// One confirmed preview item in registry order; content is retained so
/// enqueue can re-verify that the exact previewed payload is unchanged.
#[derive(Debug, Clone)]
pub struct PreviewItemData {
    pub target: AiTargetRef,
    pub skill_name: String,
    pub document_filename: Option<String>,
    pub source_hash: Option<String>,
    pub content: Option<String>,
    pub character_count: i64,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    pub eligibility: AiPreviewEligibility,
    pub error_code: Option<String>,
}

/// A confirmed preview entry: ordered targets plus the non-key configuration
/// snapshot that will be persisted into the batch on enqueue.
#[derive(Debug)]
pub struct PreviewEntry {
    pub id: String,
    pub expires_at: i64,
    pub mode: AiAnalysisMode,
    pub items: Vec<PreviewItemData>,
    pub config_snapshot: AiConfigInput,
    pub total_characters: i64,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    pub estimated_cost_micros: Option<i64>,
    pub estimated_max_retry_cost_micros: Option<i64>,
    pub total_targets: i64,
    pub valid_documents: i64,
    pub missing_documents: i64,
    pub unreadable_documents: i64,
    pub skipped_targets: i64,
}

impl PreviewEntry {
    pub fn items_dto(&self) -> Vec<AiPreviewItemDto> {
        self.items
            .iter()
            .map(|item| AiPreviewItemDto {
                target: item.target.clone(),
                skill_name: item.skill_name.clone(),
                document_filename: item.document_filename.clone(),
                source_hash: item.source_hash.clone(),
                content: item.content.clone(),
                character_count: item.character_count,
                estimated_input_tokens: item.estimated_input_tokens,
                estimated_output_tokens: item.estimated_output_tokens,
                eligibility: item.eligibility,
                error_code: item.error_code.clone(),
            })
            .collect()
    }
}

/// Register a newly built preview; expired entries are dropped opportunistically
/// so the registry cannot grow unbounded during a long session.
pub fn register_preview(
    registry: &Mutex<HashMap<String, PreviewEntry>>,
    entry: PreviewEntry,
) -> Result<String, AiCommandError> {
    let mut entries = registry.lock().map_err(|_| internal_error())?;
    let now = now_millis();
    entries.retain(|_, existing| existing.expires_at > now);
    let id = entry.id.clone();
    entries.insert(id.clone(), entry);
    Ok(id)
}

/// Atomically consume a preview id. The removal happens before any re-check or
/// database write so a failed enqueue can never replay the same preview.
pub fn consume_preview(
    registry: &Mutex<HashMap<String, PreviewEntry>>,
    preview_id: &str,
    now: i64,
) -> Result<PreviewEntry, AiCommandError> {
    let mut entries = registry.lock().map_err(|_| internal_error())?;
    let entry = entries
        .remove(preview_id)
        .ok_or_else(|| preview_error(AiErrorCode::PreviewNotFound))?;
    if entry.expires_at <= now {
        return Err(preview_error(AiErrorCode::PreviewExpired));
    }
    Ok(entry)
}

/// Build one preview item from a collected document.
pub fn item_from_document(
    target: AiTargetRef,
    document: Option<CollectedDocument>,
    outcome: DocumentOutcome,
) -> PreviewItemData {
    match (outcome, document) {
        (DocumentOutcome::Ready, Some(document)) => {
            let (input_tokens, output_tokens) = estimate_document_tokens(&document.content);
            PreviewItemData {
                target,
                skill_name: document.skill_name,
                document_filename: Some(document.document_filename),
                source_hash: Some(document.source_hash),
                content: Some(document.content),
                character_count: document.character_count,
                estimated_input_tokens: input_tokens,
                estimated_output_tokens: output_tokens,
                eligibility: AiPreviewEligibility::Ready,
                error_code: None,
            }
        }
        (DocumentOutcome::NoDocument, _) => PreviewItemData {
            target,
            skill_name: String::new(),
            document_filename: None,
            source_hash: None,
            content: None,
            character_count: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            eligibility: AiPreviewEligibility::NoDocument,
            error_code: None,
        },
        (DocumentOutcome::Unreadable { error_code }, _) => PreviewItemData {
            target,
            skill_name: String::new(),
            document_filename: None,
            source_hash: None,
            content: None,
            character_count: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            eligibility: AiPreviewEligibility::Unreadable,
            error_code: Some(error_code_name(error_code)),
        },
        _ => unreachable!("Ready outcome always carries a document"),
    }
}

/// Frozen token formula from the contract: CJK chars count 1:1, non-CJK chars
/// divide by 4 (rounded up), plus a fixed 512-token prompt overhead. Output is
/// clamped between 512 and 2048 and derived from the estimated input.
pub fn estimate_document_tokens(content: &str) -> (i64, i64) {
    let mut cjk = 0_i64;
    let mut non_cjk = 0_i64;
    for character in content.chars() {
        if is_cjk(character) {
            cjk += 1;
        } else {
            non_cjk += 1;
        }
    }
    let input = cjk
        .saturating_add(ceil_div(non_cjk, 4))
        .saturating_add(FIXED_PROMPT_TOKENS);
    let output = ceil_div(input, 4).clamp(512, MAX_OUTPUT_TOKENS);
    (input, output)
}

/// Manual ceiling division for non-negative values; keeps the estimator on the
/// project's MSRV without depending on newer integer rounding APIs.
fn ceil_div(value: i64, divisor: i64) -> i64 {
    (value + divisor - 1) / divisor
}

/// Estimate single-success and worst-case three-request cost micros using
/// checked i128 math; the result must fit i64 or the configuration is invalid.
/// `None` prices mean the UI shows token estimates without a monetary figure.
pub fn estimate_costs(
    input_tokens: i64,
    output_tokens: i64,
    config: &AiConfigInput,
) -> Result<(Option<i64>, Option<i64>), AiCommandError> {
    let Some(input_price) = config.input_price_micros_per_million else {
        return Ok((None, None));
    };
    let Some(output_price) = config.output_price_micros_per_million else {
        return Ok((None, None));
    };

    let single = checked_cost(input_tokens, input_price)?
        .checked_add(checked_cost(output_tokens, output_price)?)
        .ok_or_else(cost_overflow)?;

    // Worst case: each of up to three real HTTP requests resends the input
    // plus the fixed 2048 output reservation, per the frozen upper bound.
    let retry_input = input_tokens
        .checked_add(MAX_OUTPUT_TOKENS)
        .ok_or_else(cost_overflow)?;
    let per_retry = checked_cost(retry_input, input_price)?
        .checked_add(checked_cost(MAX_OUTPUT_TOKENS, output_price)?)
        .ok_or_else(cost_overflow)?;
    let maximum = single
        .checked_add(per_retry.checked_mul(2).ok_or_else(cost_overflow)?)
        .ok_or_else(cost_overflow)?;

    Ok((Some(single), Some(maximum)))
}

fn checked_cost(tokens: i64, price_micros_per_million: i64) -> Result<i64, AiCommandError> {
    let numerator = i128::from(tokens) * i128::from(price_micros_per_million);
    let micros = if numerator <= 0 {
        0
    } else {
        (numerator + 999_999) / 1_000_000
    };
    i64::try_from(micros).map_err(|_| cost_overflow())
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2EBEF
            | 0x30000..=0x323AF
    )
}

pub(crate) fn error_code_name(code: AiErrorCode) -> String {
    match code {
        AiErrorCode::InvalidTarget => "invalid_target",
        AiErrorCode::NoDocument => "no_document",
        AiErrorCode::UnsafePath => "unsafe_path",
        AiErrorCode::NotConfigured => "not_configured",
        AiErrorCode::KeyUnavailable => "key_unavailable",
        AiErrorCode::InvalidConfig => "invalid_config",
        AiErrorCode::InvalidBaseUrl => "invalid_base_url",
        AiErrorCode::ContentChanged => "content_changed",
        AiErrorCode::HttpAuth => "http_auth",
        AiErrorCode::HttpRequest => "http_request",
        AiErrorCode::RateLimited => "rate_limited",
        AiErrorCode::ProviderResponse => "provider_response",
        AiErrorCode::InvalidJson => "invalid_json",
        AiErrorCode::SchemaValidation => "schema_validation",
        AiErrorCode::Cancelled => "cancelled",
        AiErrorCode::Conflict => "conflict",
        AiErrorCode::DuplicateTarget => "duplicate_target",
        AiErrorCode::AmbiguousTarget => "ambiguous_target",
        AiErrorCode::UnreadableDocument => "unreadable_document",
        AiErrorCode::InvalidUtf8 => "invalid_utf8",
        AiErrorCode::DocumentTooLarge => "document_too_large",
        AiErrorCode::PreviewNotFound => "preview_not_found",
        AiErrorCode::PreviewExpired => "preview_expired",
        AiErrorCode::PreviewConsumed => "preview_consumed",
        AiErrorCode::HttpTimeout => "http_timeout",
        AiErrorCode::InvalidState => "invalid_state",
        AiErrorCode::NotFound => "not_found",
        AiErrorCode::ResponseTooLarge => "response_too_large",
        AiErrorCode::Db => "db",
        AiErrorCode::Keyring => "keyring",
        AiErrorCode::Internal => "internal",
    }
    .to_string()
}

/// Inverse of [`error_code_name`]: stable persisted error strings map back to
/// the machine-readable enum for DTOs and UI decisions.
pub(crate) fn error_code_from_name(value: &str) -> AiErrorCode {
    match value {
        "invalid_target" => AiErrorCode::InvalidTarget,
        "no_document" => AiErrorCode::NoDocument,
        "unsafe_path" => AiErrorCode::UnsafePath,
        "not_configured" => AiErrorCode::NotConfigured,
        "key_unavailable" => AiErrorCode::KeyUnavailable,
        "invalid_config" => AiErrorCode::InvalidConfig,
        "invalid_base_url" => AiErrorCode::InvalidBaseUrl,
        "content_changed" => AiErrorCode::ContentChanged,
        "http_auth" => AiErrorCode::HttpAuth,
        "http_request" => AiErrorCode::HttpRequest,
        "rate_limited" => AiErrorCode::RateLimited,
        "provider_response" => AiErrorCode::ProviderResponse,
        "invalid_json" => AiErrorCode::InvalidJson,
        "schema_validation" => AiErrorCode::SchemaValidation,
        "cancelled" => AiErrorCode::Cancelled,
        "conflict" => AiErrorCode::Conflict,
        "duplicate_target" => AiErrorCode::DuplicateTarget,
        "ambiguous_target" => AiErrorCode::AmbiguousTarget,
        "unreadable_document" => AiErrorCode::UnreadableDocument,
        "invalid_utf8" => AiErrorCode::InvalidUtf8,
        "document_too_large" => AiErrorCode::DocumentTooLarge,
        "preview_not_found" => AiErrorCode::PreviewNotFound,
        "preview_expired" => AiErrorCode::PreviewExpired,
        "preview_consumed" => AiErrorCode::PreviewConsumed,
        "http_timeout" => AiErrorCode::HttpTimeout,
        "invalid_state" => AiErrorCode::InvalidState,
        "not_found" => AiErrorCode::NotFound,
        "response_too_large" => AiErrorCode::ResponseTooLarge,
        "db" => AiErrorCode::Db,
        "keyring" => AiErrorCode::Keyring,
        _ => AiErrorCode::Internal,
    }
}

pub fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

pub fn new_preview_id() -> String {
    Uuid::new_v4().to_string()
}

fn preview_error(code: AiErrorCode) -> AiCommandError {
    command_error(
        AiErrorKind::State,
        code,
        "The AI analysis preview is missing, expired, or already consumed.",
        false,
    )
}

fn cost_overflow() -> AiCommandError {
    command_error(
        AiErrorKind::Configuration,
        AiErrorCode::InvalidConfig,
        "The configured AI price exceeds the supported cost range.",
        false,
    )
}

fn internal_error() -> AiCommandError {
    command_error(
        AiErrorKind::Internal,
        AiErrorCode::Internal,
        "The AI preview registry is unavailable.",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AiConfigInput {
        AiConfigInput {
            provider: "openai".into(),
            base_url: "https://example.invalid/v1/".into(),
            model: "model".into(),
            output_language: "en".into(),
            timeout_seconds: 60,
            concurrency: 1,
            log_retention_days: 30,
            input_price_micros_per_million: Some(1_000_000),
            output_price_micros_per_million: Some(2_000_000),
        }
    }

    #[test]
    fn token_formula_counts_cjk_and_non_cjk_separately() {
        let (input, output) = estimate_document_tokens("中文");
        // 2 CJK + 0 non-CJK + 512 prompt overhead.
        assert_eq!(input, 514);
        assert_eq!(output, 512);

        let (input, output) = estimate_document_tokens("abcd");
        // 4 non-CJK / 4 = 1 + 512.
        assert_eq!(input, 513);
        assert_eq!(output, 512);
    }

    #[test]
    fn output_tokens_are_clamped_and_derived() {
        // 5000 CJK chars -> input 5512 -> output ceil(5512/4)=1378.
        let content = "中".repeat(5_000);
        let (input, output) = estimate_document_tokens(&content);
        assert_eq!(input, 5_512);
        assert_eq!(output, 1_378);
    }

    #[test]
    fn cost_estimate_uses_single_and_three_request_bounds() {
        let (single, maximum) = estimate_costs(512, 512, &config()).unwrap();
        // input 512 * 1 micro = 512; output 512 * 2 micro = 1024.
        assert_eq!(single, Some(1_536));
        // retry per request: input 512+2048=2560 *1 + output 2048*2 = 2560+4096=6656
        // maximum = 1536 + 6656*2 = 14848
        assert_eq!(maximum, Some(14_848));
    }

    #[test]
    fn missing_prices_produce_null_amounts() {
        let mut config = config();
        config.input_price_micros_per_million = None;
        assert_eq!(estimate_costs(512, 512, &config).unwrap(), (None, None));
    }

    #[test]
    fn cost_overflow_returns_invalid_config() {
        let mut config = config();
        config.input_price_micros_per_million = Some(1_000_000_000_000_000);
        config.output_price_micros_per_million = Some(1_000_000_000_000_000);
        let result = estimate_costs(i64::MAX - 1, i64::MAX - 1, &config);
        assert_eq!(result.unwrap_err().code, AiErrorCode::InvalidConfig);
    }

    #[test]
    fn preview_registry_consumes_once_and_expires() {
        let registry: Mutex<HashMap<String, PreviewEntry>> = Mutex::new(HashMap::new());
        let entry = PreviewEntry {
            id: "preview-1".into(),
            expires_at: now_millis() + 60_000,
            mode: AiAnalysisMode::MissingOrStale,
            items: Vec::new(),
            config_snapshot: config(),
            total_characters: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            estimated_cost_micros: None,
            estimated_max_retry_cost_micros: None,
            total_targets: 0,
            valid_documents: 0,
            missing_documents: 0,
            unreadable_documents: 0,
            skipped_targets: 0,
        };
        register_preview(&registry, entry).unwrap();

        let consumed = consume_preview(&registry, "preview-1", now_millis()).unwrap();
        assert_eq!(consumed.id, "preview-1");
        assert_eq!(
            consume_preview(&registry, "preview-1", now_millis())
                .unwrap_err()
                .code,
            AiErrorCode::PreviewNotFound
        );

        let expired = PreviewEntry {
            id: "preview-expired".into(),
            expires_at: now_millis() - 1,
            mode: AiAnalysisMode::MissingOrStale,
            items: Vec::new(),
            config_snapshot: config(),
            total_characters: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            estimated_cost_micros: None,
            estimated_max_retry_cost_micros: None,
            total_targets: 0,
            valid_documents: 0,
            missing_documents: 0,
            unreadable_documents: 0,
            skipped_targets: 0,
        };
        register_preview(&registry, expired).unwrap();
        assert_eq!(
            consume_preview(&registry, "preview-expired", now_millis())
                .unwrap_err()
                .code,
            AiErrorCode::PreviewExpired
        );
    }
}
