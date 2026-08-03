use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
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
    canonical_target, target_ref_from_payload, AiRepository, TargetState,
};
use crate::core::ai::runner::AiRuntimeState;
use crate::core::ai::secret_store::SecretStore;
use crate::core::ai::types::{
    AiAnalysisDetailDto, AiAnalysisMode, AiAnalysisPreviewDto, AiAnalysisResultV1,
    AiAnalysisStatus, AiAnalysisSummaryDto, AiApiKeyStatusDto, AiBatchDto, AiBatchRecord,
    AiBatchStatus, AiCommandError, AiConfigDto, AiConfigInput, AiConnectionTestDto,
    AiConnectionTestInput, AiErrorCode, AiErrorKind, AiJobRecord, AiJobStatus,
    AiPreviewEligibility, AiProviderPresetDto, AiTargetRef,
};
use crate::core::skill_store::SkillStore;

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
