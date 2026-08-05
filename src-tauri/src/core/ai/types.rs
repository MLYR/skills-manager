use serde::{Deserialize, Serialize};
use std::fmt;

/// Public targets remain a tagged union so callers cannot inject an absolute
/// path or a precomputed database key across the command boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AiTargetRef {
    Managed {
        skill_id: String,
    },
    GlobalLocal {
        agent_key: String,
        relative_path: String,
    },
    ProjectLocal {
        project_id: String,
        agent_key: String,
        relative_path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiTargetKind {
    Managed,
    GlobalLocal,
    ProjectLocal,
}

impl AiTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::GlobalLocal => "global_local",
            Self::ProjectLocal => "project_local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiErrorKind {
    Validation,
    Configuration,
    Security,
    Provider,
    State,
    Storage,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiErrorCode {
    InvalidTarget,
    NoDocument,
    UnsafePath,
    NotConfigured,
    KeyUnavailable,
    InvalidConfig,
    InvalidBaseUrl,
    ContentChanged,
    HttpAuth,
    HttpRequest,
    RateLimited,
    ProviderResponse,
    InvalidJson,
    SchemaValidation,
    Cancelled,
    Conflict,
    DuplicateTarget,
    AmbiguousTarget,
    UnreadableDocument,
    InvalidUtf8,
    DocumentTooLarge,
    PreviewNotFound,
    PreviewExpired,
    PreviewConsumed,
    HttpTimeout,
    InvalidState,
    NotFound,
    ResponseTooLarge,
    Db,
    Keyring,
    Internal,
}

/// Commands expose stable machine-readable recovery metadata; messages are
/// deliberately not the contract for deciding whether an operation retries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiCommandError {
    pub kind: AiErrorKind,
    pub code: AiErrorCode,
    pub message: String,
    pub retryable: bool,
    pub next_retry_at: Option<i64>,
}

impl fmt::Display for AiCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AiCommandError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiConfigInput {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub output_language: String,
    pub timeout_seconds: u32,
    pub concurrency: u8,
    pub log_retention_days: u16,
    // 兼容旧配置读取，但新配置不再保存或使用价格字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_micros_per_million: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_micros_per_million: Option<i64>,
}

/// 配置 DTO 返回本地保存的 Key，设置页需要据此回显掩码并支持替换；AI日志和任务 DTO 仍不携带它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiConfigDto {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub output_language: String,
    pub timeout_seconds: u32,
    pub concurrency: u8,
    pub log_retention_days: u16,
    pub api_key: Option<String>,
    pub has_api_key: bool,
    pub is_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiApiKeyStatusDto {
    pub has_api_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiProviderPresetDto {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    pub default_model: Option<String>,
    pub api_key_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiModelDto {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiModelListInput {
    pub config: AiConfigInput,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiConnectionTestInput {
    pub config: AiConfigInput,
    #[serde(default)]
    pub api_key: Option<String>,
    pub confirm_billable_request: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiConnectionTestDto {
    pub success: bool,
    pub provider: String,
    pub model: String,
    pub message: String,
    pub http_status: Option<i64>,
    pub latency_ms: i64,
    pub billable_request_sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiAnalysisResultV1 {
    pub one_line: String,
    pub what_it_does: String,
    pub when_to_use: Vec<String>,
    pub how_to_use: Vec<String>,
    pub example_prompts: Vec<String>,
    pub requirements: Vec<String>,
    pub not_for: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPreviewEligibility {
    Ready,
    NoDocument,
    Unreadable,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAnalysisMode {
    MissingOnly,
    StaleOnly,
    MissingOrStale,
    Force,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiPreviewItemDto {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiAnalysisPreviewDto {
    pub preview_id: String,
    pub expires_at: i64,
    pub mode: AiAnalysisMode,
    pub total_targets: i64,
    pub valid_documents: i64,
    pub missing_documents: i64,
    pub unreadable_documents: i64,
    pub skipped_targets: i64,
    pub total_characters: i64,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub output_language: String,
    pub items: Vec<AiPreviewItemDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAnalysisStatus {
    Unconfigured,
    Unparsed,
    Queued,
    Running,
    Paused,
    Failed,
    Succeeded,
    Stale,
    NoDocument,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiBatchStatus {
    Queued,
    Running,
    Paused,
    Cancelling,
    Completed,
    Cancelled,
}

impl AiBatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiJobStatus {
    Queued,
    Running,
    RetryWait,
    Interrupted,
    Succeeded,
    Failed,
    Cancelled,
}

impl AiJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::RetryWait => "retry_wait",
            Self::Interrupted => "interrupted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiLogEventKind {
    RequestStarted,
    ResponseReceived,
    RequestFailed,
    RetryScheduled,
    CorrectionRequested,
    Recovery,
    Cancelled,
}

impl AiLogEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestStarted => "request_started",
            Self::ResponseReceived => "response_received",
            Self::RequestFailed => "request_failed",
            Self::RetryScheduled => "retry_scheduled",
            Self::CorrectionRequested => "correction_requested",
            Self::Recovery => "recovery",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiJobDto {
    pub id: String,
    pub batch_id: String,
    pub ordinal: i64,
    pub target: AiTargetRef,
    pub skill_name: String,
    pub status: AiJobStatus,
    pub attempt_count: i64,
    pub manual_retry_count: i64,
    pub correction_attempted: bool,
    pub cancel_requested: bool,
    pub next_retry_at: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiAnalysisDetailDto {
    pub target: AiTargetRef,
    pub status: AiAnalysisStatus,
    pub skill_name: Option<String>,
    pub source_hash: Option<String>,
    pub current_source_hash: Option<String>,
    pub schema_version: Option<i64>,
    pub prompt_version: Option<String>,
    pub output_language: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub one_line: Option<String>,
    pub result: Option<AiAnalysisResultV1>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub analyzed_at: Option<i64>,
    pub active_job: Option<AiJobDto>,
    pub last_error: Option<AiCommandError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiAnalysisSummaryDto {
    pub target: AiTargetRef,
    pub skill_name: String,
    pub status: AiAnalysisStatus,
    pub one_line: Option<String>,
    pub when_to_use: Vec<String>,
    pub source_hash: Option<String>,
    pub is_stale: bool,
    pub updated_at: Option<i64>,
    pub active_job_id: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiBatchDto {
    pub id: String,
    pub status: AiBatchStatus,
    pub total_targets: i64,
    pub valid_documents: i64,
    pub missing_documents: i64,
    pub unreadable_documents: i64,
    pub skipped_targets: i64,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    pub jobs_queued: i64,
    pub jobs_running: i64,
    pub jobs_retry_wait: i64,
    pub jobs_interrupted: i64,
    pub jobs_succeeded: i64,
    pub jobs_failed: i64,
    pub jobs_cancelled: i64,
    pub progress_completed: i64,
    pub progress_total: i64,
    pub pause_requested: bool,
    pub cancel_requested: bool,
    pub confirmed_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiQueueStatsDto {
    pub targets_total: i64,
    pub targets_unparsed: i64,
    pub targets_succeeded: i64,
    pub targets_stale: i64,
    pub targets_failed: i64,
    pub targets_no_document: i64,
    pub targets_unreadable: i64,
    pub batches_queued: i64,
    pub batches_running: i64,
    pub batches_paused: i64,
    pub batches_cancelling: i64,
    pub batches_completed: i64,
    pub batches_cancelled: i64,
    pub jobs_queued: i64,
    pub jobs_running: i64,
    pub jobs_retry_wait: i64,
    pub jobs_interrupted: i64,
    pub jobs_succeeded: i64,
    pub jobs_failed: i64,
    pub jobs_cancelled: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiLogSummaryDto {
    pub id: String,
    pub event_kind: String,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub target: Option<AiTargetRef>,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_code: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiLogDetailDto {
    pub id: String,
    pub event_kind: String,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub target: Option<AiTargetRef>,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_code: Option<String>,
    pub created_at: i64,
    pub request_system_prompt: Option<String>,
    pub request_user_prompt: Option<String>,
    pub raw_response: Option<String>,
    pub error_message: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiJobListInput {
    pub batch_id: Option<String>,
    pub status: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiBatchListInput {
    pub status: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AiLogListInput {
    pub event_kind: Option<String>,
    pub error_code: Option<String>,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiJobPageDto {
    pub items: Vec<AiJobDto>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiBatchPageDto {
    pub items: Vec<AiBatchDto>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AiLogPageDto {
    pub items: Vec<AiLogSummaryDto>,
    pub next_cursor: Option<String>,
}

// Database records mirror the frozen v8 columns without introducing any
// credential/header field, keeping the storage boundary auditable in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiAnalysisRecord {
    pub id: String,
    pub target_kind: AiTargetKind,
    pub target_key: String,
    pub target_payload_json: String,
    pub skill_name: String,
    pub source_hash: String,
    pub schema_version: i64,
    pub prompt_version: String,
    pub output_language: String,
    pub one_line: String,
    pub result_json: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub analyzed_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiBatchRecord {
    pub id: String,
    pub status: AiBatchStatus,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub output_language: String,
    pub prompt_version: String,
    pub schema_version: i64,
    pub timeout_seconds: i64,
    pub input_price_micros_per_million: Option<i64>,
    pub output_price_micros_per_million: Option<i64>,
    pub estimated_input_tokens: i64,
    pub estimated_output_tokens: i64,
    pub estimated_cost_micros: Option<i64>,
    pub estimated_max_retry_cost_micros: Option<i64>,
    pub total_targets: i64,
    pub valid_documents: i64,
    pub missing_documents: i64,
    pub unreadable_documents: i64,
    pub skipped_targets: i64,
    pub pause_requested: bool,
    pub cancel_requested: bool,
    pub confirmed_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiJobRecord {
    pub id: String,
    pub batch_id: String,
    pub ordinal: i64,
    pub target_kind: AiTargetKind,
    pub target_key: String,
    pub target_payload_json: String,
    pub skill_name: String,
    pub expected_source_hash: String,
    pub status: AiJobStatus,
    pub priority: i64,
    pub attempt_count: i64,
    pub manual_retry_count: i64,
    pub correction_attempted: bool,
    pub cancel_requested: bool,
    pub next_retry_at: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiLogRecord {
    pub id: String,
    pub event_kind: AiLogEventKind,
    pub job_id: Option<String>,
    pub batch_id: Option<String>,
    pub target_kind: Option<AiTargetKind>,
    pub target_key: Option<String>,
    pub target_payload_json: Option<String>,
    pub skill_name: Option<String>,
    pub request_system_prompt: Option<String>,
    pub request_user_prompt: Option<String>,
    pub raw_response: Option<String>,
    pub http_status: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ref_uses_internal_snake_case_kind_tag() {
        let target = AiTargetRef::ProjectLocal {
            project_id: "project-1".into(),
            agent_key: "codex".into(),
            relative_path: "nested/Skill".into(),
        };
        let value = serde_json::to_value(target).unwrap();

        assert_eq!(value["kind"], "project_local");
        assert_eq!(value["relative_path"], "nested/Skill");
        assert!(value.get("target_key").is_none());
    }

    #[test]
    fn config_dto_exposes_local_key_for_masked_settings_edit() {
        let dto = AiConfigDto {
            provider: "openai".into(),
            base_url: "https://example.invalid/v1/".into(),
            model: "model".into(),
            output_language: "en".into(),
            timeout_seconds: 60,
            concurrency: 1,
            log_retention_days: 30,
            api_key: Some("test-key".into()),
            has_api_key: true,
            is_configured: true,
        };
        let value = serde_json::to_value(dto).unwrap();

        assert_eq!(value["has_api_key"], true);
        assert_eq!(value["api_key"], "test-key");
        assert!(value.get("masked_api_key").is_none());
    }

    #[test]
    fn command_error_serializes_machine_readable_snake_case_values() {
        let error = AiCommandError {
            kind: AiErrorKind::Storage,
            code: AiErrorCode::PreviewNotFound,
            message: "preview missing".into(),
            retryable: false,
            next_retry_at: None,
        };
        let value = serde_json::to_value(error).unwrap();

        assert_eq!(value["kind"], "storage");
        assert_eq!(value["code"], "preview_not_found");
        assert_eq!(value["next_retry_at"], serde_json::Value::Null);
    }
}
