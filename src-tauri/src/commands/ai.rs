use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::core::ai::command_error;
use crate::core::ai::config::{
    load_config, provider_presets, provider_requires_api_key, save_config, to_dto,
    validate_connection_config,
};
use crate::core::ai::document::{collect_document, CollectedDocument, DocumentOutcome};
use crate::core::ai::preview::{
    consume_preview, error_code_from_name, estimate_costs, item_from_document, new_preview_id,
    now_millis, register_preview, PreviewEntry, PREVIEW_TTL_MILLIS,
};
use crate::core::ai::prompt::PROMPT_VERSION;
use crate::core::ai::provider::{connection_message, send_minimal_completion, ProviderAttempt};
use crate::core::ai::repository::{
    canonical_target, target_ref_from_payload, AiRepository, CancelJobOutcome, TargetState,
};
use crate::core::ai::runner::AiRuntimeState;
use crate::core::ai::secret_store::SecretStore;
use crate::core::ai::types::{
    AiAnalysisDetailDto, AiAnalysisMode, AiAnalysisPreviewDto, AiAnalysisResultV1,
    AiAnalysisStatus, AiAnalysisSummaryDto, AiApiKeyStatusDto, AiBatchDto, AiBatchListInput,
    AiBatchPageDto, AiBatchRecord, AiBatchStatus, AiCommandError, AiConfigDto, AiConfigInput,
    AiConnectionTestDto, AiConnectionTestInput, AiErrorCode, AiErrorKind, AiJobListInput,
    AiJobPageDto, AiJobRecord, AiJobStatus, AiLogDetailDto, AiLogListInput, AiLogPageDto,
    AiLogRecord, AiLogSummaryDto, AiPreviewEligibility, AiProviderPresetDto, AiQueueStatsDto,
    AiTargetRef,
};
use crate::core::project_scanner;
use crate::core::skill_store::SkillStore;
use crate::core::tool_adapters;
use std::path::Path;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct SetAiApiKeyInput {
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PreviewAiAnalysisInput {
    targets: Vec<AiTargetRef>,
    mode: AiAnalysisMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct EnqueueAiAnalysisInput {
    preview_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct GetAiAnalysisInput {
    target: AiTargetRef,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ListAiAnalysisSummariesInput {
    targets: Vec<AiTargetRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PauseAiBatchInput {
    batch_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ResumeAiBatchInput {
    batch_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct CancelAiBatchInput {
    batch_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct CancelAiJobInput {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct GetAiBatchInput {
    batch_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct RetryAiJobInput {
    job_id: String,
    preview_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct GetAiLogInput {
    log_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ClearAiLogsDto {
    deleted_count: i64,
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

#[tauri::command]
pub async fn preview_ai_analysis(
    input: PreviewAiAnalysisInput,
    store: State<'_, Arc<SkillStore>>,
    state: State<'_, Arc<AiRuntimeState>>,
) -> Result<AiAnalysisPreviewDto, AiCommandError> {
    validate_target_inputs(&input.targets)?;
    // Preview requires a valid configuration so the non-key snapshot (provider,
    // base URL, model, prices, timeout) is exactly what the batch will commit.
    let config = load_config(store.inner())?;
    validate_connection_config(&config)?;
    let store = store.inner().clone();
    let state = state.inner().clone();
    run_blocking(move || build_preview(&store, &state, &input.targets, input.mode, &config)).await
}

#[tauri::command]
pub async fn enqueue_ai_analysis(
    input: EnqueueAiAnalysisInput,
    store: State<'_, Arc<SkillStore>>,
    state: State<'_, Arc<AiRuntimeState>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    let state = state.inner().clone();
    run_blocking(move || enqueue_preview(&store, &state, &input.preview_id)).await
}

#[tauri::command]
pub async fn get_ai_analysis(
    input: GetAiAnalysisInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiAnalysisDetailDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || build_analysis_detail(&store, &input.target)).await
}

#[tauri::command]
pub async fn list_ai_analysis_summaries(
    input: ListAiAnalysisSummariesInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<AiAnalysisSummaryDto>, AiCommandError> {
    if input.targets.is_empty() {
        return Ok(Vec::new());
    }
    validate_target_inputs(&input.targets)?;
    let store = store.inner().clone();
    run_blocking(move || {
        let config = load_config(&store).ok();
        let repository = AiRepository::new(&store);
        input
            .targets
            .into_iter()
            .map(|target| {
                let (outcome, document) = collect_document(&store, &target);
                let state = repository_result(repository.get_target_state(&target))?;
                Ok(build_summary_dto(
                    &target,
                    &outcome,
                    document.as_ref(),
                    config.as_ref(),
                    &state,
                ))
            })
            .collect()
    })
    .await
}

#[tauri::command]
pub async fn list_ai_analysis_batches(
    input: AiBatchListInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiBatchPageDto, AiCommandError> {
    validate_batch_status_filter(input.status.as_deref())?;
    if input.limit == 0 || input.limit > 100 {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidState,
            "AI batch list limit must be between 1 and 100.",
            false,
        ));
    }
    let store = store.inner().clone();
    run_blocking(move || {
        let repository = AiRepository::new(&store);
        let (batches, next_cursor) = repository_result(repository.list_batches(
            input.status.as_deref(),
            input.cursor.as_deref(),
            input.limit,
        ))?;
        let items = batches
            .iter()
            .map(|batch| to_batch_dto(&store, batch))
            .collect::<Result<Vec<_>, AiCommandError>>()?;
        Ok(AiBatchPageDto { items, next_cursor })
    })
    .await
}

#[tauri::command]
pub async fn list_ai_analysis_jobs(
    input: AiJobListInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiJobPageDto, AiCommandError> {
    validate_job_status_filter(input.status.as_deref())?;
    if input.limit == 0 || input.limit > 100 {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidState,
            "AI job list limit must be between 1 and 100.",
            false,
        ));
    }
    let store = store.inner().clone();
    run_blocking(move || {
        let repository = AiRepository::new(&store);
        let (jobs, next_cursor) = repository_result(repository.list_jobs(
            input.batch_id.as_deref(),
            input.status.as_deref(),
            input.cursor.as_deref(),
            input.limit,
        ))?;
        let items = jobs
            .iter()
            .map(job_to_dto)
            .collect::<Result<Vec<_>, AiCommandError>>()?;
        Ok(AiJobPageDto { items, next_cursor })
    })
    .await
}

#[tauri::command]
pub async fn get_ai_analysis_batch(
    input: GetAiBatchInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let batch = repository_result(AiRepository::new(&store).load_batch_dto(&input.batch_id))?
            .ok_or_else(|| {
            command_error(
                AiErrorKind::State,
                AiErrorCode::NotFound,
                "The AI analysis batch does not exist.",
                false,
            )
        })?;
        to_batch_dto(&store, &batch)
    })
    .await
}

#[tauri::command]
pub async fn get_ai_analysis_queue_stats(
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiQueueStatsDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || build_queue_stats(&store)).await
}

#[tauri::command]
pub async fn pause_ai_analysis_batch(
    input: PauseAiBatchInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let now = now_millis();
        let batch = repository_result(AiRepository::new(&store).pause_batch(&input.batch_id, now))?
            .ok_or_else(|| not_found_error("batch"))?;
        if batch.status != AiBatchStatus::Paused {
            return Err(command_error(
                AiErrorKind::State,
                AiErrorCode::InvalidState,
                "Only queued or running batches can be paused.",
                false,
            ));
        }
        to_batch_dto(&store, &batch)
    })
    .await
}

#[tauri::command]
pub async fn resume_ai_analysis_batch(
    input: ResumeAiBatchInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let now = now_millis();
        let batch =
            repository_result(AiRepository::new(&store).resume_batch(&input.batch_id, now))?
                .ok_or_else(|| not_found_error("batch"))?;
        if !matches!(
            batch.status,
            AiBatchStatus::Queued | AiBatchStatus::Completed
        ) {
            return Err(command_error(
                AiErrorKind::State,
                AiErrorCode::InvalidState,
                "Only paused batches can be resumed.",
                false,
            ));
        }
        to_batch_dto(&store, &batch)
    })
    .await
}

#[tauri::command]
pub async fn cancel_ai_analysis_batch(
    input: CancelAiBatchInput,
    store: State<'_, Arc<SkillStore>>,
    state: State<'_, Arc<AiRuntimeState>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    let state = state.inner().clone();
    run_blocking(move || {
        let now = now_millis();
        let (batch, running_jobs) =
            repository_result(AiRepository::new(&store).cancel_batch(&input.batch_id, now))?;
        let batch = batch.ok_or_else(|| not_found_error("batch"))?;
        for job_id in running_jobs {
            state.request_cancel(&job_id);
        }
        to_batch_dto(&store, &batch)
    })
    .await
}

#[tauri::command]
pub async fn cancel_ai_analysis_job(
    input: CancelAiJobInput,
    store: State<'_, Arc<SkillStore>>,
    state: State<'_, Arc<AiRuntimeState>>,
) -> Result<crate::core::ai::types::AiJobDto, AiCommandError> {
    let store = store.inner().clone();
    let state = state.inner().clone();
    run_blocking(move || {
        let now = now_millis();
        let repository = AiRepository::new(&store);
        let outcome = repository_result(repository.cancel_job(&input.job_id, now))?;
        match outcome {
            CancelJobOutcome::InvalidState => Err(command_error(
                AiErrorKind::State,
                AiErrorCode::InvalidState,
                "Only queued, retry-waiting, interrupted, running, or cancelled jobs support cancellation.",
                false,
            )),
            CancelJobOutcome::RunningCancelled => {
                state.request_cancel(&input.job_id);
                let job = repository_result(repository.get_job(&input.job_id))?
                    .ok_or_else(|| not_found_error("job"))?;
                job_to_dto(&job)
            }
            CancelJobOutcome::Cancelled => {
                let job = repository_result(repository.get_job(&input.job_id))?
                    .ok_or_else(|| not_found_error("job"))?;
                job_to_dto(&job)
            }
        }
    })
    .await
}

#[tauri::command]
pub async fn retry_ai_analysis_job(
    input: RetryAiJobInput,
    store: State<'_, Arc<SkillStore>>,
    state: State<'_, Arc<AiRuntimeState>>,
) -> Result<AiBatchDto, AiCommandError> {
    let store = store.inner().clone();
    let state = state.inner().clone();
    run_blocking(move || {
        // Atomic consumption: a retry preview can only be used once, exactly
        // like a first-time batch confirmation.
        let entry = consume_preview(&state.previews, &input.preview_id, now_millis())?;
        let repository = AiRepository::new(&store);
        let job = repository_result(repository.get_job(&input.job_id))?
            .ok_or_else(|| not_found_error("job"))?;
        if job.status != AiJobStatus::Failed {
            return Err(command_error(
                AiErrorKind::State,
                AiErrorCode::InvalidState,
                "Only failed jobs can be manually retried.",
                false,
            ));
        }
        if entry.items.len() != 1 || entry.mode != AiAnalysisMode::Force {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::PreviewConsumed,
                "A manual retry requires a single-target force preview.",
                false,
            ));
        }
        let item = &entry.items[0];
        let (kind, key, payload) = canonical_target(&item.target);
        if (kind, key.as_str()) != (job.target_kind, job.target_key.as_str()) {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::PreviewConsumed,
                "The retry preview target does not match the failed job.",
                false,
            ));
        }
        let Some(source_hash) = item.source_hash.clone() else {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::PreviewConsumed,
                "The retry preview has no document hash.",
                false,
            ));
        };

        let now = now_millis();
        let batch_id = uuid::Uuid::new_v4().to_string();
        let new_job = AiJobRecord {
            id: uuid::Uuid::new_v4().to_string(),
            batch_id: batch_id.clone(),
            ordinal: 0,
            target_kind: kind,
            target_key: job.target_key,
            target_payload_json: payload,
            skill_name: item.skill_name.clone(),
            expected_source_hash: source_hash,
            status: AiJobStatus::Queued,
            priority: 0,
            attempt_count: 0,
            manual_retry_count: job.manual_retry_count + 1,
            correction_attempted: false,
            cancel_requested: false,
            next_retry_at: None,
            error_code: None,
            error_message: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        };
        let batch = AiBatchRecord {
            id: batch_id,
            status: AiBatchStatus::Queued,
            provider: entry.config_snapshot.provider.clone(),
            base_url: entry.config_snapshot.base_url.clone(),
            model: entry.config_snapshot.model.clone(),
            output_language: resolve_output_language(&entry.config_snapshot.output_language),
            prompt_version: PROMPT_VERSION.to_string(),
            schema_version: 1,
            timeout_seconds: i64::from(entry.config_snapshot.timeout_seconds),
            input_price_micros_per_million: entry.config_snapshot.input_price_micros_per_million,
            output_price_micros_per_million: entry.config_snapshot.output_price_micros_per_million,
            estimated_input_tokens: entry.estimated_input_tokens,
            estimated_output_tokens: entry.estimated_output_tokens,
            estimated_cost_micros: entry.estimated_cost_micros,
            estimated_max_retry_cost_micros: entry.estimated_max_retry_cost_micros,
            total_targets: 1,
            valid_documents: 1,
            missing_documents: 0,
            unreadable_documents: 0,
            skipped_targets: 0,
            pause_requested: false,
            cancel_requested: false,
            confirmed_at: now,
            created_at: now,
            updated_at: now,
            finished_at: None,
        };
        repository_result(repository.insert_batch_with_jobs(&batch, &[new_job]))?;
        to_batch_dto(&store, &batch)
    })
    .await
}

#[tauri::command]
pub async fn list_ai_analysis_logs(
    input: AiLogListInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiLogPageDto, AiCommandError> {
    validate_log_event_kind_filter(input.event_kind.as_deref())?;
    if input.limit == 0 || input.limit > 100 {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidState,
            "AI log list limit must be between 1 and 100.",
            false,
        ));
    }
    let store = store.inner().clone();
    run_blocking(move || {
        let (logs, next_cursor) = repository_result(AiRepository::new(&store).list_logs(
            input.event_kind.as_deref(),
            input.error_code.as_deref(),
            input.job_id.as_deref(),
            input.batch_id.as_deref(),
            input.cursor.as_deref(),
            input.limit,
        ))?;
        let items = logs
            .iter()
            .map(log_to_summary)
            .collect::<Result<Vec<_>, AiCommandError>>()?;
        Ok(AiLogPageDto { items, next_cursor })
    })
    .await
}

#[tauri::command]
pub async fn get_ai_analysis_log(
    input: GetAiLogInput,
    store: State<'_, Arc<SkillStore>>,
) -> Result<AiLogDetailDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let log = repository_result(AiRepository::new(&store).get_log(&input.log_id))?
            .ok_or_else(|| not_found_error("log"))?;
        log_to_detail(&log)
    })
    .await
}

#[tauri::command]
pub async fn clear_ai_analysis_logs(
    store: State<'_, Arc<SkillStore>>,
) -> Result<ClearAiLogsDto, AiCommandError> {
    let store = store.inner().clone();
    run_blocking(move || {
        let deleted_count = crate::core::ai::logs::clear_logs(&store)?;
        Ok(ClearAiLogsDto {
            deleted_count: i64::try_from(deleted_count).unwrap_or(i64::MAX),
        })
    })
    .await
}

fn log_to_summary(log: &AiLogRecord) -> Result<AiLogSummaryDto, AiCommandError> {
    Ok(AiLogSummaryDto {
        id: log.id.clone(),
        event_kind: log.event_kind.as_str().to_string(),
        job_id: log.job_id.clone(),
        batch_id: log.batch_id.clone(),
        target: log_target(&log.target_payload_json),
        http_status: log.http_status,
        duration_ms: log.duration_ms,
        error_code: log.error_code.clone(),
        created_at: log.created_at,
    })
}

fn log_to_detail(log: &AiLogRecord) -> Result<AiLogDetailDto, AiCommandError> {
    Ok(AiLogDetailDto {
        id: log.id.clone(),
        event_kind: log.event_kind.as_str().to_string(),
        job_id: log.job_id.clone(),
        batch_id: log.batch_id.clone(),
        target: log_target(&log.target_payload_json),
        http_status: log.http_status,
        duration_ms: log.duration_ms,
        error_code: log.error_code.clone(),
        created_at: log.created_at,
        request_system_prompt: log.request_system_prompt.clone(),
        request_user_prompt: log.request_user_prompt.clone(),
        raw_response: log.raw_response.clone(),
        error_message: log.error_message.clone(),
        input_tokens: log.input_tokens,
        output_tokens: log.output_tokens,
        total_tokens: log.total_tokens,
    })
}

fn log_target(payload: &Option<String>) -> Option<AiTargetRef> {
    payload
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok())
}

fn validate_log_event_kind_filter(event_kind: Option<&str>) -> Result<(), AiCommandError> {
    if let Some(event_kind) = event_kind {
        if !matches!(
            event_kind,
            "request_started"
                | "response_received"
                | "request_failed"
                | "retry_scheduled"
                | "correction_requested"
                | "recovery"
                | "cancelled"
        ) {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::InvalidState,
                "Invalid AI log event kind filter.",
                false,
            ));
        }
    }
    Ok(())
}

fn build_queue_stats(store: &SkillStore) -> Result<AiQueueStatsDto, AiCommandError> {
    let repository = AiRepository::new(store);
    let (batch_counts, job_counts) = repository_result(repository.queue_counts())?;
    let jobs_cancelled = repository_result(repository.cancelled_job_count())?;
    let config = load_config(store).ok();

    let mut targets = Vec::new();
    let managed = store
        .get_all_skills()
        .map_err(|_| storage_error("list managed skills"))?;
    targets.extend(
        managed
            .into_iter()
            .map(|skill| AiTargetRef::Managed { skill_id: skill.id }),
    );
    for adapter in tool_adapters::all_tool_adapters(store) {
        let skills = project_scanner::read_linked_workspace_skills(
            &adapter.skills_dir(),
            None,
            &adapter.key,
            &adapter.display_name,
            adapter.recursive_scan,
        );
        targets.extend(skills.into_iter().map(|skill| AiTargetRef::GlobalLocal {
            agent_key: skill.agent,
            relative_path: skill.relative_path,
        }));
    }
    let projects = store
        .get_all_projects()
        .map_err(|_| storage_error("list workspaces"))?;
    for record in projects {
        if record.workspace_type == "linked" {
            let agent_key = record
                .linked_agent_key
                .clone()
                .unwrap_or_else(|| crate::commands::projects::slugify_skill_dir_name(&record.name));
            let agent_name = record
                .linked_agent_name
                .clone()
                .unwrap_or_else(|| record.name.clone());
            let skills = project_scanner::read_linked_workspace_skills(
                Path::new(&record.path),
                record.disabled_path.as_deref().map(Path::new),
                &agent_key,
                &agent_name,
                true,
            );
            targets.extend(skills.into_iter().map(|skill| AiTargetRef::ProjectLocal {
                project_id: record.id.clone(),
                agent_key: skill.agent,
                relative_path: skill.relative_path,
            }));
        } else {
            let configs = crate::core::ai::document::agent_scan_configs(store);
            let skills = project_scanner::read_project_skills(Path::new(&record.path), &configs);
            targets.extend(skills.into_iter().map(|skill| AiTargetRef::ProjectLocal {
                project_id: record.id.clone(),
                agent_key: skill.agent,
                relative_path: skill.relative_path,
            }));
        }
    }

    let mut seen = std::collections::HashSet::new();
    targets.retain(|target| {
        let (_, key, _) = canonical_target(target);
        seen.insert(key)
    });

    let mut total = 0_i64;
    let mut unparsed = 0_i64;
    let mut succeeded = 0_i64;
    let mut stale = 0_i64;
    let mut failed = 0_i64;
    let mut no_document = 0_i64;
    let mut unreadable = 0_i64;
    for target in &targets {
        total += 1;
        let (outcome, document) = collect_document(store, target);
        let (kind, key, _) = canonical_target(target);
        let state = repository_result(repository.get_target_state_by_key(&kind, &key))?;
        let status = compute_status(
            &outcome,
            config.as_ref(),
            &state,
            document.as_ref().map(|doc| doc.source_hash.as_str()),
        );
        match status {
            AiAnalysisStatus::Unparsed | AiAnalysisStatus::Unconfigured => unparsed += 1,
            AiAnalysisStatus::Succeeded => succeeded += 1,
            AiAnalysisStatus::Stale => stale += 1,
            AiAnalysisStatus::Failed => failed += 1,
            AiAnalysisStatus::NoDocument => no_document += 1,
            AiAnalysisStatus::Unreadable => unreadable += 1,
            AiAnalysisStatus::Queued | AiAnalysisStatus::Running | AiAnalysisStatus::Paused => {
                unparsed += 1
            }
        }
    }

    Ok(AiQueueStatsDto {
        targets_total: total,
        targets_unparsed: unparsed,
        targets_succeeded: succeeded,
        targets_stale: stale,
        targets_failed: failed,
        targets_no_document: no_document,
        targets_unreadable: unreadable,
        batches_queued: batch_counts.0,
        batches_running: batch_counts.1,
        batches_paused: batch_counts.2,
        batches_cancelling: batch_counts.3,
        batches_completed: batch_counts.4,
        batches_cancelled: batch_counts.5,
        jobs_queued: job_counts.0,
        jobs_running: job_counts.1,
        jobs_retry_wait: job_counts.2,
        jobs_interrupted: job_counts.3,
        jobs_succeeded: job_counts.4,
        jobs_failed: job_counts.5,
        jobs_cancelled,
    })
}

fn validate_batch_status_filter(status: Option<&str>) -> Result<(), AiCommandError> {
    if let Some(status) = status {
        if !matches!(
            status,
            "queued" | "running" | "paused" | "cancelling" | "completed" | "cancelled"
        ) {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::InvalidState,
                "Invalid AI batch status filter.",
                false,
            ));
        }
    }
    Ok(())
}

fn validate_job_status_filter(status: Option<&str>) -> Result<(), AiCommandError> {
    if let Some(status) = status {
        if !matches!(
            status,
            "queued"
                | "running"
                | "retry_wait"
                | "interrupted"
                | "succeeded"
                | "failed"
                | "cancelled"
        ) {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::InvalidState,
                "Invalid AI job status filter.",
                false,
            ));
        }
    }
    Ok(())
}

fn not_found_error(subject: &str) -> AiCommandError {
    command_error(
        AiErrorKind::State,
        AiErrorCode::NotFound,
        format!("The AI analysis {subject} does not exist."),
        false,
    )
}

fn storage_error(operation: &str) -> AiCommandError {
    command_error(
        AiErrorKind::Storage,
        AiErrorCode::Db,
        format!("Unable to {operation}."),
        true,
    )
}

fn build_preview(
    store: &SkillStore,
    state: &AiRuntimeState,
    targets: &[AiTargetRef],
    mode: AiAnalysisMode,
    config: &AiConfigInput,
) -> Result<AiAnalysisPreviewDto, AiCommandError> {
    let mut items = Vec::with_capacity(targets.len());
    let mut total_characters = 0_i64;
    let mut input_tokens = 0_i64;
    let mut output_tokens = 0_i64;
    let mut valid = 0_i64;
    let mut missing = 0_i64;
    let mut unreadable = 0_i64;

    for target in targets {
        let (outcome, document) = collect_document(store, target);
        match outcome {
            DocumentOutcome::Ready => valid += 1,
            DocumentOutcome::NoDocument => missing += 1,
            DocumentOutcome::Unreadable { .. } => unreadable += 1,
        }
        let item = item_from_document(target.clone(), document, outcome);
        if item.eligibility == AiPreviewEligibility::Ready {
            total_characters = total_characters.saturating_add(item.character_count);
            input_tokens = input_tokens.saturating_add(item.estimated_input_tokens);
            output_tokens = output_tokens.saturating_add(item.estimated_output_tokens);
        }
        items.push(item);
    }

    let (cost, maximum_cost) = estimate_costs(input_tokens, output_tokens, config)?;
    let output_language = resolve_output_language(&config.output_language);
    let now = now_millis();
    let entry = PreviewEntry {
        id: new_preview_id(),
        expires_at: now.saturating_add(PREVIEW_TTL_MILLIS),
        mode,
        items,
        config_snapshot: config.clone(),
        total_characters,
        estimated_input_tokens: input_tokens,
        estimated_output_tokens: output_tokens,
        estimated_cost_micros: cost,
        estimated_max_retry_cost_micros: maximum_cost,
        total_targets: targets.len() as i64,
        valid_documents: valid,
        missing_documents: missing,
        unreadable_documents: unreadable,
        skipped_targets: 0,
    };
    let preview_id = register_preview(&state.previews, entry)?;
    let expires_at = now.saturating_add(PREVIEW_TTL_MILLIS);
    let items_dto = state
        .previews
        .lock()
        .map_err(|_| {
            command_error(
                AiErrorKind::Internal,
                AiErrorCode::Internal,
                "The AI preview registry is unavailable.",
                true,
            )
        })?
        .get(&preview_id)
        .map(PreviewEntry::items_dto)
        .unwrap_or_default();
    Ok(AiAnalysisPreviewDto {
        preview_id,
        expires_at,
        mode,
        total_targets: targets.len() as i64,
        valid_documents: valid,
        missing_documents: missing,
        unreadable_documents: unreadable,
        skipped_targets: 0,
        total_characters,
        estimated_input_tokens: input_tokens,
        estimated_output_tokens: output_tokens,
        estimated_cost_micros: cost,
        estimated_max_retry_cost_micros: maximum_cost,
        provider: config.provider.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        output_language: output_language.clone(),
        items: items_dto,
    })
}

fn enqueue_preview(
    store: &SkillStore,
    state: &AiRuntimeState,
    preview_id: &str,
) -> Result<AiBatchDto, AiCommandError> {
    // Consumption is atomic and irreversible: even a failed enqueue cannot
    // replay the same confirmed preview.
    let entry = consume_preview(&state.previews, preview_id, now_millis())?;
    let repository = AiRepository::new(store);
    let mut jobs = Vec::new();

    for (ordinal, item) in entry.items.iter().enumerate() {
        let (outcome, document) = collect_document(store, &item.target);
        match (outcome, document) {
            (DocumentOutcome::Ready, Some(document)) => {
                // The hash is the authorization boundary: any change after the
                // user confirmed stops the whole batch before a single byte is
                // sent, and no rows are written.
                if item.source_hash.as_deref() != Some(document.source_hash.as_str()) {
                    return Err(content_changed_error());
                }
                let (kind, key, payload) = canonical_target(&item.target);
                let now = now_millis();
                jobs.push(AiJobRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    batch_id: String::new(), // filled after batch id is created
                    ordinal: ordinal as i64,
                    target_kind: kind,
                    target_key: key,
                    target_payload_json: payload,
                    skill_name: document.skill_name,
                    expected_source_hash: document.source_hash,
                    status: AiJobStatus::Queued,
                    priority: 0,
                    attempt_count: 0,
                    manual_retry_count: 0,
                    correction_attempted: false,
                    cancel_requested: false,
                    next_retry_at: None,
                    error_code: None,
                    error_message: None,
                    created_at: now,
                    updated_at: now,
                    started_at: None,
                    finished_at: None,
                });
            }
            _ => {
                return Err(command_error(
                    AiErrorKind::State,
                    AiErrorCode::ContentChanged,
                    "The skill document changed after the preview; please preview again.",
                    false,
                ));
            }
        }
    }

    if jobs.is_empty() {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidState,
            "The preview contains no readable documents.",
            false,
        ));
    }

    let now = now_millis();
    let batch_id = uuid::Uuid::new_v4().to_string();
    for job in &mut jobs {
        job.batch_id = batch_id.clone();
    }
    let batch = AiBatchRecord {
        id: batch_id,
        status: AiBatchStatus::Queued,
        provider: entry.config_snapshot.provider.clone(),
        base_url: entry.config_snapshot.base_url.clone(),
        model: entry.config_snapshot.model.clone(),
        output_language: resolve_output_language(&entry.config_snapshot.output_language),
        prompt_version: PROMPT_VERSION.to_string(),
        schema_version: 1,
        timeout_seconds: i64::from(entry.config_snapshot.timeout_seconds),
        input_price_micros_per_million: entry.config_snapshot.input_price_micros_per_million,
        output_price_micros_per_million: entry.config_snapshot.output_price_micros_per_million,
        estimated_input_tokens: entry.estimated_input_tokens,
        estimated_output_tokens: entry.estimated_output_tokens,
        estimated_cost_micros: entry.estimated_cost_micros,
        estimated_max_retry_cost_micros: entry.estimated_max_retry_cost_micros,
        total_targets: entry.total_targets,
        valid_documents: entry.valid_documents,
        missing_documents: entry.missing_documents,
        unreadable_documents: entry.unreadable_documents,
        skipped_targets: entry.skipped_targets,
        pause_requested: false,
        cancel_requested: false,
        confirmed_at: now,
        created_at: now,
        updated_at: now,
        finished_at: None,
    };
    repository_result(repository.insert_batch_with_jobs(&batch, &jobs))?;
    to_batch_dto(store, &batch)
}

fn build_analysis_detail(
    store: &SkillStore,
    target: &AiTargetRef,
) -> Result<AiAnalysisDetailDto, AiCommandError> {
    let (outcome, document) = collect_document(store, target);
    let config = load_config(store).ok();
    let state = repository_result(AiRepository::new(store).get_target_state(target))?;
    let current_hash = document.as_ref().map(|doc| doc.source_hash.as_str());
    let status = compute_status(&outcome, config.as_ref(), &state, current_hash);
    let analysis = state.analysis.as_ref();
    let result = analysis
        .and_then(|record| serde_json::from_str::<AiAnalysisResultV1>(&record.result_json).ok());
    let active_job = state.active_job.as_ref().map(job_to_dto).transpose()?;
    let last_error = state.latest_failed_job.as_ref().map(|job| {
        let code = job
            .error_code
            .as_deref()
            .map(error_code_from_name)
            .unwrap_or(AiErrorCode::HttpRequest);
        command_error(
            AiErrorKind::Provider,
            code,
            job.error_message.clone().unwrap_or_default(),
            false,
        )
    });
    Ok(AiAnalysisDetailDto {
        target: target.clone(),
        status,
        skill_name: analysis
            .map(|record| record.skill_name.clone())
            .or_else(|| document.as_ref().map(|doc| doc.skill_name.clone())),
        source_hash: analysis.map(|record| record.source_hash.clone()),
        current_source_hash: current_hash.map(str::to_string),
        schema_version: analysis.map(|record| record.schema_version),
        prompt_version: analysis.map(|record| record.prompt_version.clone()),
        output_language: analysis.map(|record| record.output_language.clone()),
        provider: analysis.map(|record| record.provider.clone()),
        model: analysis.map(|record| record.model.clone()),
        one_line: analysis.map(|record| record.one_line.clone()),
        result,
        input_tokens: analysis.and_then(|record| record.input_tokens),
        output_tokens: analysis.and_then(|record| record.output_tokens),
        total_tokens: analysis.and_then(|record| record.total_tokens),
        analyzed_at: analysis.map(|record| record.analyzed_at),
        active_job,
        last_error,
    })
}

fn build_summary_dto(
    target: &AiTargetRef,
    outcome: &DocumentOutcome,
    document: Option<&CollectedDocument>,
    config: Option<&AiConfigInput>,
    state: &TargetState,
) -> AiAnalysisSummaryDto {
    let status = compute_status(
        outcome,
        config,
        state,
        document.map(|doc| doc.source_hash.as_str()),
    );
    let analysis = state.analysis.as_ref();
    let result = analysis
        .and_then(|record| serde_json::from_str::<AiAnalysisResultV1>(&record.result_json).ok());
    AiAnalysisSummaryDto {
        target: target.clone(),
        skill_name: analysis
            .map(|record| record.skill_name.clone())
            .or_else(|| document.map(|doc| doc.skill_name.clone()))
            .unwrap_or_default(),
        status,
        one_line: analysis.map(|record| record.one_line.clone()),
        when_to_use: result.map(|result| result.when_to_use).unwrap_or_default(),
        source_hash: analysis.map(|record| record.source_hash.clone()),
        is_stale: status == AiAnalysisStatus::Stale,
        updated_at: analysis.map(|record| record.updated_at),
        active_job_id: state.active_job.as_ref().map(|job| job.id.clone()),
        error_code: state
            .latest_failed_job
            .as_ref()
            .and_then(|job| job.error_code.clone()),
        error_message: state
            .latest_failed_job
            .as_ref()
            .and_then(|job| job.error_message.clone()),
    }
}

fn compute_status(
    outcome: &DocumentOutcome,
    config: Option<&AiConfigInput>,
    state: &TargetState,
    current_source_hash: Option<&str>,
) -> AiAnalysisStatus {
    match outcome {
        DocumentOutcome::NoDocument => return AiAnalysisStatus::NoDocument,
        DocumentOutcome::Unreadable { .. } => return AiAnalysisStatus::Unreadable,
        DocumentOutcome::Ready => {}
    }

    if let Some(job) = &state.active_job {
        if state.active_batch_paused {
            return AiAnalysisStatus::Paused;
        }
        return match job.status {
            AiJobStatus::Running => AiAnalysisStatus::Running,
            AiJobStatus::Queued | AiJobStatus::RetryWait | AiJobStatus::Interrupted => {
                AiAnalysisStatus::Queued
            }
            AiJobStatus::Succeeded | AiJobStatus::Failed | AiJobStatus::Cancelled => {
                AiAnalysisStatus::Unparsed
            }
        };
    }

    if let Some(failed) = &state.latest_failed_job {
        let newer_than_success = state
            .analysis
            .as_ref()
            .map(|analysis| failed.created_at > analysis.updated_at)
            .unwrap_or(true);
        if newer_than_success {
            return AiAnalysisStatus::Failed;
        }
    }

    if let Some(analysis) = &state.analysis {
        let current_language = config
            .map(|config| resolve_output_language(&config.output_language))
            .unwrap_or_else(|| analysis.output_language.clone());
        let fresh = analysis.source_hash == current_source_hash.unwrap_or_default()
            && analysis.schema_version == 1
            && analysis.prompt_version == PROMPT_VERSION
            && analysis.output_language == current_language;
        return if fresh {
            AiAnalysisStatus::Succeeded
        } else {
            AiAnalysisStatus::Stale
        };
    }

    if config.is_none() {
        return AiAnalysisStatus::Unconfigured;
    }
    AiAnalysisStatus::Unparsed
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

fn validate_target_inputs(targets: &[AiTargetRef]) -> Result<(), AiCommandError> {
    if targets.is_empty() {
        return Err(command_error(
            AiErrorKind::Validation,
            AiErrorCode::InvalidTarget,
            "At least one AI analysis target is required.",
            false,
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for target in targets {
        let (_, key, _) = canonical_target(target);
        if !seen.insert(key) {
            return Err(command_error(
                AiErrorKind::Validation,
                AiErrorCode::DuplicateTarget,
                "The same AI analysis target appears more than once.",
                false,
            ));
        }
    }
    Ok(())
}

/// Resolve the `auto` output language at confirmation time so the batch
/// snapshot always carries a concrete language. The UI follows the system
/// locale, so the same heuristic is used here; a missing locale defaults to
/// Chinese because the application UI defaults to Chinese.
fn resolve_output_language(value: &str) -> String {
    if value != "auto" {
        return value.to_string();
    }
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if locale.contains("zh_tw") || locale.contains("zh-tw") || locale.contains("zh_hant") {
        "zh-TW".to_string()
    } else if locale.contains("zh") {
        "zh".to_string()
    } else if locale.contains("en") {
        "en".to_string()
    } else {
        "zh".to_string()
    }
}

fn to_batch_dto(store: &SkillStore, batch: &AiBatchRecord) -> Result<AiBatchDto, AiCommandError> {
    let (queued, running, retry_wait, interrupted, succeeded, failed) = AiRepository::new(store)
        .batch_job_counts(&batch.id)
        .map_err(|_| {
            command_error(
                AiErrorKind::Storage,
                AiErrorCode::Db,
                "Unable to read the AI batch state.",
                true,
            )
        })?;
    let cancelled = batch
        .valid_documents
        .saturating_sub(queued + running + retry_wait + interrupted + succeeded + failed);
    let completed = succeeded + failed + cancelled;
    Ok(AiBatchDto {
        id: batch.id.clone(),
        status: batch.status,
        total_targets: batch.total_targets,
        valid_documents: batch.valid_documents,
        missing_documents: batch.missing_documents,
        unreadable_documents: batch.unreadable_documents,
        skipped_targets: batch.skipped_targets,
        estimated_input_tokens: batch.estimated_input_tokens,
        estimated_output_tokens: batch.estimated_output_tokens,
        estimated_cost_micros: batch.estimated_cost_micros,
        estimated_max_retry_cost_micros: batch.estimated_max_retry_cost_micros,
        jobs_queued: queued,
        jobs_running: running,
        jobs_retry_wait: retry_wait,
        jobs_interrupted: interrupted,
        jobs_succeeded: succeeded,
        jobs_failed: failed,
        jobs_cancelled: cancelled,
        progress_completed: completed,
        progress_total: batch.valid_documents,
        pause_requested: batch.pause_requested,
        cancel_requested: batch.cancel_requested,
        confirmed_at: batch.confirmed_at,
        created_at: batch.created_at,
        updated_at: batch.updated_at,
        finished_at: batch.finished_at,
    })
}

fn job_to_dto(job: &AiJobRecord) -> Result<crate::core::ai::types::AiJobDto, AiCommandError> {
    let target = target_ref_from_payload(&job.target_payload_json).map_err(|_| {
        command_error(
            AiErrorKind::Internal,
            AiErrorCode::Internal,
            "Stored AI job target payload is invalid.",
            false,
        )
    })?;
    Ok(crate::core::ai::types::AiJobDto {
        id: job.id.clone(),
        batch_id: job.batch_id.clone(),
        ordinal: job.ordinal,
        target,
        skill_name: job.skill_name.clone(),
        status: job.status,
        attempt_count: job.attempt_count,
        manual_retry_count: job.manual_retry_count,
        correction_attempted: job.correction_attempted,
        cancel_requested: job.cancel_requested,
        next_retry_at: job.next_retry_at,
        error_code: job.error_code.clone(),
        error_message: job.error_message.clone(),
        created_at: job.created_at,
        updated_at: job.updated_at,
        started_at: job.started_at,
        finished_at: job.finished_at,
    })
}

fn content_changed_error() -> AiCommandError {
    command_error(
        AiErrorKind::State,
        AiErrorCode::ContentChanged,
        "The skill document changed after the preview; please preview again.",
        false,
    )
}

fn repository_result<T>(result: anyhow::Result<T>) -> Result<T, AiCommandError> {
    result.map_err(|error| {
        command_error(
            AiErrorKind::Storage,
            AiErrorCode::Db,
            format!("AI analysis database operation failed: {error}"),
            true,
        )
    })
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

    #[test]
    fn phase2_command_inputs_reject_unknown_or_misnested_fields() {
        assert!(
            serde_json::from_value::<PreviewAiAnalysisInput>(serde_json::json!({
                "targets": [{"kind":"managed","skill_id":"s"}],
                "mode": "missing_only",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PreviewAiAnalysisInput>(serde_json::json!({
                "target": {"kind":"managed","skill_id":"s"},
                "mode": "missing_only"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<EnqueueAiAnalysisInput>(serde_json::json!({
                "preview_id": "p",
                "extra": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<GetAiAnalysisInput>(serde_json::json!({
                "target": {"kind":"managed","skill_id":"s","unknown":1}
            }))
            .is_err()
        );
    }

    #[test]
    fn duplicate_targets_are_rejected_before_preview() {
        let targets = vec![
            AiTargetRef::Managed {
                skill_id: "s".into(),
            },
            AiTargetRef::Managed {
                skill_id: "s".into(),
            },
        ];
        assert_eq!(
            validate_target_inputs(&targets).unwrap_err().code,
            AiErrorCode::DuplicateTarget
        );
    }
}
