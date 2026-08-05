//! Single-job analysis orchestration: re-locate the skill, verify the previewed
//! hash, build the untrusted-data prompt, spend one pre-committed HTTP attempt,
//! validate the response, and persist result/log/job/batch in one transaction.
//!
//! Every blocking operation (filesystem and SQLite) runs on a blocking
//! thread via `spawn_blocking` so the async runner never stalls a Tokio
//! worker; only the HTTP attempt itself is awaited on the runtime.

use std::sync::Arc;

use crate::core::skill_store::SkillStore;

use super::config::{load_api_key, load_config, provider_requires_api_key};
use super::document::{collect_document, CollectedDocument, DocumentOutcome};
use super::logs::{sanitize_error_message, sanitized_record};
use super::preview::now_millis;
use super::prompt::{build_analysis_prompt, AiAnalysisPrompt, PROMPT_VERSION};
use super::provider::{send_analysis_completion, AnalysisAttempt};
use super::repository::{
    target_ref_from_payload, AiRepository, AttemptReservation, ClaimedJob, CompleteOutcome,
    FailOutcome,
};
use super::runner::AiRuntimeState;
use super::schema::validate_ai_analysis_result_v1;
use super::types::{
    AiAnalysisRecord, AiCommandError, AiConfigInput, AiErrorCode, AiErrorKind, AiJobRecord,
    AiLogEventKind, AiLogRecord,
};

/// Everything the async loop needs after the blocking preparation phase.
struct PreparedJob {
    document: CollectedDocument,
    prompt: AiAnalysisPrompt,
    config: AiConfigInput,
    api_key: Option<String>,
    proxy_url: Option<String>,
}

/// Top-level runner entry. Errors are logged, never panicked into the task.
pub async fn process_job(store: Arc<SkillStore>, state: Arc<AiRuntimeState>, claimed: ClaimedJob) {
    let job_id = claimed.job.id.clone();
    if let Err(error) = run_job(store, &state, &claimed).await {
        log::warn!("AI analysis job {job_id} stopped unexpectedly: {error}");
    }
    state.unregister_running(&job_id);
}

async fn run_job(
    store: Arc<SkillStore>,
    state: &AiRuntimeState,
    claimed: &ClaimedJob,
) -> Result<(), AiCommandError> {
    let job = &claimed.job;
    let job_id = job.id.clone();
    if state.is_cancelled(&job_id) {
        return Ok(());
    }

    // Blocking preparation: re-locate, hash re-check, config, local key, prompt.
    // `None` means the job already reached a terminal state during prep.
    let prepared = {
        let store = store.clone();
        let claimed = claimed.clone();
        tokio::task::spawn_blocking(move || prepare_job(&store, &claimed))
            .await
            .map_err(|_| internal_error("AI analysis preparation task failed"))??
    };
    let Some(prepared) = prepared else {
        return Ok(());
    };

    let mut correction = false;
    loop {
        if state.is_cancelled(&job_id) {
            return Ok(());
        }
        let now = now_millis();
        let started_log = sanitized_record(
            request_started_log(
                job,
                &claimed.batch,
                &prepared.document,
                &prepared.prompt.system_prompt,
                &prepared.prompt.user_prompt,
                now,
            ),
            prepared.api_key.as_deref(),
        );
        let reservation = {
            let store = store.clone();
            let job_id = job_id.clone();
            tokio::task::spawn_blocking(move || {
                AiRepository::new(&store).reserve_http_attempt(
                    &job_id,
                    correction,
                    now,
                    started_log,
                )
            })
            .await
            .map_err(|_| internal_error("AI analysis reservation task failed"))?
            .map_err(|_| storage_error("reserve an AI analysis attempt"))?
        };

        match reservation {
            AttemptReservation::Cancelled => return Ok(()),
            AttemptReservation::NoBudget => {
                fail_terminal(
                    store.clone(),
                    job,
                    "provider_response",
                    "The analysis exceeded the confirmed maximum of three requests.",
                    now,
                    None,
                )
                .await?;
                return Ok(());
            }
            AttemptReservation::Reserved { attempt_number } => {
                let attempt = send_analysis_completion(
                    &prepared.config,
                    prepared.api_key.as_deref(),
                    prepared.proxy_url.as_deref(),
                    &prepared.prompt.system_prompt,
                    &prepared.prompt.user_prompt,
                )
                .await?;
                match attempt {
                    AnalysisAttempt::Failed {
                        code,
                        message,
                        retryable,
                        ..
                    } => {
                        let sanitized =
                            sanitize_error_message(&message, prepared.api_key.as_deref());
                        let now = now_millis();
                        if retryable && attempt_number < 3 {
                            fail_retry(
                                store.clone(),
                                job,
                                &error_code_name(code),
                                &sanitized,
                                backoff_seconds(attempt_number),
                                prepared.api_key.as_deref(),
                                now,
                            )
                            .await?;
                        } else {
                            fail_terminal(
                                store.clone(),
                                job,
                                &error_code_name(code),
                                &sanitized,
                                now,
                                prepared.api_key.as_deref(),
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                    AnalysisAttempt::Response {
                        status,
                        body,
                        retry_after_secs,
                        latency_ms,
                    } => {
                        if status.is_success() {
                            // OpenAI-compatible chat completions wrap the model
                            // text in choices[0].message.content; validating the
                            // envelope itself always fails schema v1. Extract
                            // the inner content first, then validate it.
                            match extract_analysis_payload(&body)
                                .and_then(|payload| validate_ai_analysis_result_v1(&payload))
                            {
                                Ok(result) => {
                                    let now = now_millis();
                                    let (input, output, total) = extract_usage(&body);
                                    let log = sanitized_record(
                                        response_received_log(
                                            job,
                                            &claimed.batch,
                                            &body,
                                            status.as_u16(),
                                            latency_ms,
                                            input,
                                            output,
                                            total,
                                            now,
                                        ),
                                        prepared.api_key.as_deref(),
                                    );
                                    let analysis = build_analysis_record(
                                        job,
                                        &claimed.batch,
                                        &prepared.document,
                                        &result,
                                        input,
                                        output,
                                        total,
                                        now,
                                    )?;
                                    let outcome = {
                                        let store = store.clone();
                                        let job_id = job_id.clone();
                                        tokio::task::spawn_blocking(move || {
                                            AiRepository::new(&store)
                                                .complete_success(&job_id, &analysis, log, now)
                                        })
                                        .await
                                        .map_err(|_| {
                                            internal_error("AI analysis commit task failed")
                                        })?
                                        .map_err(
                                            |_| storage_error("commit an AI analysis result"),
                                        )?
                                    };
                                    match outcome {
                                        CompleteOutcome::Succeeded => return Ok(()),
                                        CompleteOutcome::Cancelled => return Ok(()),
                                    }
                                }
                                Err(schema_error)
                                    if !correction
                                        && matches!(
                                            schema_error.code,
                                            AiErrorCode::InvalidJson
                                                | AiErrorCode::SchemaValidation
                                        ) =>
                                {
                                    // One correction request is allowed by the
                                    // confirmed cost ceiling; the counter is
                                    // pre-committed on the next loop iteration.
                                    correction = true;
                                    continue;
                                }
                                Err(schema_error) => {
                                    let now = now_millis();
                                    fail_terminal(
                                        store.clone(),
                                        job,
                                        &error_code_name(schema_error.code),
                                        &schema_error.message,
                                        now,
                                        prepared.api_key.as_deref(),
                                    )
                                    .await?;
                                    return Ok(());
                                }
                            }
                        } else if status.as_u16() == 429 {
                            let now = now_millis();
                            let wait = retry_after_secs
                                .map(|seconds| i64::try_from(seconds).unwrap_or(60))
                                .unwrap_or(backoff_seconds(attempt_number));
                            if attempt_number < 3 {
                                fail_retry(
                                    store.clone(),
                                    job,
                                    "rate_limited",
                                    "The AI provider rate limit was reached.",
                                    wait,
                                    prepared.api_key.as_deref(),
                                    now,
                                )
                                .await?;
                            } else {
                                fail_terminal(
                                    store.clone(),
                                    job,
                                    "rate_limited",
                                    "The AI provider rate limit was reached.",
                                    now,
                                    prepared.api_key.as_deref(),
                                )
                                .await?;
                            }
                            return Ok(());
                        } else if status.as_u16() == 408 || status.is_server_error() {
                            let now = now_millis();
                            let wait = backoff_seconds(attempt_number);
                            let code = if status.as_u16() == 408 {
                                "http_timeout"
                            } else {
                                "provider_response"
                            };
                            let message = if status.as_u16() == 408 {
                                "The AI provider request timed out."
                            } else {
                                "The AI provider is temporarily unavailable."
                            };
                            if attempt_number < 3 {
                                fail_retry(
                                    store.clone(),
                                    job,
                                    code,
                                    message,
                                    wait,
                                    prepared.api_key.as_deref(),
                                    now,
                                )
                                .await?;
                            } else {
                                fail_terminal(
                                    store.clone(),
                                    job,
                                    code,
                                    message,
                                    now,
                                    prepared.api_key.as_deref(),
                                )
                                .await?;
                            }
                            return Ok(());
                        } else {
                            let now = now_millis();
                            let code = if matches!(status.as_u16(), 401 | 403) {
                                "http_auth"
                            } else {
                                "provider_response"
                            };
                            let message = if matches!(status.as_u16(), 401 | 403) {
                                "The AI provider rejected the API key or permission."
                            } else {
                                "The AI provider rejected the analysis request."
                            };
                            fail_terminal(
                                store.clone(),
                                job,
                                code,
                                message,
                                now,
                                prepared.api_key.as_deref(),
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// Blocking preparation phase; returns `Ok(None)` when the job was already
/// finalized (missing/unreadable/changed document, missing key).
fn prepare_job(
    store: &SkillStore,
    claimed: &ClaimedJob,
) -> Result<Option<PreparedJob>, AiCommandError> {
    let job = &claimed.job;
    let batch = &claimed.batch;

    let target = target_ref_from_payload(&job.target_payload_json)
        .map_err(|_| internal_error("stored target payload is invalid"))?;
    let (outcome, document) = collect_document(store, &target);
    let document = match (outcome, document) {
        (DocumentOutcome::Ready, Some(document)) => document,
        (DocumentOutcome::NoDocument, _) => {
            fail_terminal_sync(
                store,
                job,
                "no_document",
                "No main document was found.",
                now_millis(),
                None,
            )?;
            return Ok(None);
        }
        (DocumentOutcome::Unreadable { error_code }, _) => {
            let code = error_code_name(error_code);
            fail_terminal_sync(
                store,
                job,
                &code,
                &format!("The skill document is not readable ({code})."),
                now_millis(),
                None,
            )?;
            return Ok(None);
        }
        (_, _) => {
            fail_terminal_sync(
                store,
                job,
                "unreadable_document",
                "The skill document is not readable.",
                now_millis(),
                None,
            )?;
            return Ok(None);
        }
    };

    // The previewed hash is the authorization boundary: content changed after
    // preview is a terminal failure, never a re-send of new content.
    if document.source_hash != job.expected_source_hash {
        fail_terminal_sync(
            store,
            job,
            "content_changed",
            "The skill document changed after the analysis preview; please preview again.",
            now_millis(),
            None,
        )?;
        return Ok(None);
    }

    // The batch snapshot fixes request-affecting fields; the current global
    // config supplies non-batch policy values so validation stays identical.
    let current = load_config(store)?;
    let config = AiConfigInput {
        provider: batch.provider.clone(),
        base_url: batch.base_url.clone(),
        model: batch.model.clone(),
        output_language: batch.output_language.clone(),
        timeout_seconds: batch.timeout_seconds as u32,
        concurrency: current.concurrency,
        log_retention_days: current.log_retention_days,
        input_price_micros_per_million: batch.input_price_micros_per_million,
        output_price_micros_per_million: batch.output_price_micros_per_million,
    };
    let proxy_url = store
        .get_setting("proxy_url")
        .map_err(|_| storage_error("read the proxy configuration"))?;
    let api_key = if provider_requires_api_key(&batch.provider) {
        let key = load_api_key(store)?;
        if key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .is_none()
        {
            fail_terminal_sync(
                store,
                job,
                "key_unavailable",
                "The provider API key is missing; configure it in Settings.",
                now_millis(),
                None,
            )?;
            return Ok(None);
        }
        key
    } else {
        None
    };

    let prompt = build_analysis_prompt(&batch.output_language, &document.content)?;
    Ok(Some(PreparedJob {
        document,
        prompt,
        config,
        api_key,
        proxy_url,
    }))
}

/// Async wrapper that runs the terminal-failure write on a blocking thread.
async fn fail_terminal(
    store: Arc<SkillStore>,
    job: &AiJobRecord,
    error_code: &str,
    message: &str,
    now: i64,
    api_key: Option<&str>,
) -> Result<(), AiCommandError> {
    let job = job.clone();
    let error_code = error_code.to_string();
    let message = message.to_string();
    let api_key = api_key.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        fail_terminal_sync(&store, &job, &error_code, &message, now, api_key.as_deref())
    })
    .await
    .map_err(|_| internal_error("AI analysis failure write task failed"))??;
    Ok(())
}

async fn fail_retry(
    store: Arc<SkillStore>,
    job: &AiJobRecord,
    error_code: &str,
    message: &str,
    wait_seconds: i64,
    api_key: Option<&str>,
    now: i64,
) -> Result<(), AiCommandError> {
    let job = job.clone();
    let error_code = error_code.to_string();
    let message = message.to_string();
    let api_key = api_key.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        fail_retry_sync(
            &store,
            &job,
            &error_code,
            &message,
            wait_seconds,
            api_key.as_deref(),
            now,
        )
    })
    .await
    .map_err(|_| internal_error("AI analysis retry write task failed"))??;
    Ok(())
}

fn fail_terminal_sync(
    store: &SkillStore,
    job: &AiJobRecord,
    error_code: &str,
    message: &str,
    now: i64,
    api_key: Option<&str>,
) -> Result<(), AiCommandError> {
    let log = sanitized_record(
        AiLogRecord {
            id: uuid::Uuid::new_v4().to_string(),
            event_kind: AiLogEventKind::RequestFailed,
            job_id: Some(job.id.clone()),
            batch_id: Some(job.batch_id.clone()),
            target_kind: Some(job.target_kind),
            target_key: Some(job.target_key.clone()),
            target_payload_json: Some(job.target_payload_json.clone()),
            skill_name: Some(job.skill_name.clone()),
            request_system_prompt: None,
            request_user_prompt: None,
            raw_response: None,
            http_status: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            duration_ms: None,
            error_code: Some(error_code.into()),
            error_message: Some(message.into()),
            created_at: now,
        },
        api_key,
    );
    repo_result(AiRepository::new(store).fail_job(&job.id, error_code, message, None, log, now))?;
    Ok(())
}

fn fail_retry_sync(
    store: &SkillStore,
    job: &AiJobRecord,
    error_code: &str,
    message: &str,
    wait_seconds: i64,
    api_key: Option<&str>,
    now: i64,
) -> Result<(), AiCommandError> {
    let log = sanitized_record(
        AiLogRecord {
            id: uuid::Uuid::new_v4().to_string(),
            event_kind: AiLogEventKind::RetryScheduled,
            job_id: Some(job.id.clone()),
            batch_id: Some(job.batch_id.clone()),
            target_kind: Some(job.target_kind),
            target_key: Some(job.target_key.clone()),
            target_payload_json: Some(job.target_payload_json.clone()),
            skill_name: Some(job.skill_name.clone()),
            request_system_prompt: None,
            request_user_prompt: None,
            raw_response: None,
            http_status: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            duration_ms: None,
            error_code: Some(error_code.into()),
            error_message: Some(message.into()),
            created_at: now,
        },
        api_key,
    );
    match repo_result(AiRepository::new(store).fail_job(
        &job.id,
        error_code,
        message,
        Some(wait_seconds),
        log,
        now,
    ))? {
        FailOutcome::RetryScheduled(_) => Ok(()),
        FailOutcome::Failed | FailOutcome::Cancelled => Ok(()),
    }
}

fn request_started_log(
    job: &AiJobRecord,
    batch: &super::types::AiBatchRecord,
    document: &CollectedDocument,
    system_prompt: &str,
    user_prompt: &str,
    now: i64,
) -> AiLogRecord {
    AiLogRecord {
        id: uuid::Uuid::new_v4().to_string(),
        event_kind: AiLogEventKind::RequestStarted,
        job_id: Some(job.id.clone()),
        batch_id: Some(batch.id.clone()),
        target_kind: Some(job.target_kind),
        target_key: Some(job.target_key.clone()),
        target_payload_json: Some(job.target_payload_json.clone()),
        skill_name: Some(document.skill_name.clone()),
        request_system_prompt: Some(system_prompt.to_string()),
        request_user_prompt: Some(user_prompt.to_string()),
        raw_response: None,
        http_status: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        duration_ms: None,
        error_code: None,
        error_message: None,
        created_at: now,
    }
}

fn response_received_log(
    job: &AiJobRecord,
    batch: &super::types::AiBatchRecord,
    body: &[u8],
    http_status: u16,
    latency_ms: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    now: i64,
) -> AiLogRecord {
    AiLogRecord {
        id: uuid::Uuid::new_v4().to_string(),
        event_kind: AiLogEventKind::ResponseReceived,
        job_id: Some(job.id.clone()),
        batch_id: Some(batch.id.clone()),
        target_kind: Some(job.target_kind),
        target_key: Some(job.target_key.clone()),
        target_payload_json: Some(job.target_payload_json.clone()),
        skill_name: Some(job.skill_name.clone()),
        request_system_prompt: None,
        request_user_prompt: None,
        raw_response: Some(String::from_utf8_lossy(body).into_owned()),
        http_status: Some(i64::from(http_status)),
        input_tokens,
        output_tokens,
        total_tokens,
        duration_ms: Some(latency_ms),
        error_code: None,
        error_message: None,
        created_at: now,
    }
}

fn build_analysis_record(
    job: &AiJobRecord,
    batch: &super::types::AiBatchRecord,
    document: &CollectedDocument,
    result: &super::types::AiAnalysisResultV1,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    now: i64,
) -> Result<AiAnalysisRecord, AiCommandError> {
    let result_json = serde_json::to_string(result)
        .map_err(|_| internal_error("failed to serialize the AI analysis result"))?;
    Ok(AiAnalysisRecord {
        id: uuid::Uuid::new_v4().to_string(),
        target_kind: job.target_kind,
        target_key: job.target_key.clone(),
        target_payload_json: job.target_payload_json.clone(),
        skill_name: document.skill_name.clone(),
        source_hash: document.source_hash.clone(),
        schema_version: 1,
        prompt_version: PROMPT_VERSION.to_string(),
        output_language: batch.output_language.clone(),
        one_line: result.one_line.clone(),
        result_json,
        provider: batch.provider.clone(),
        model: batch.model.clone(),
        input_tokens,
        output_tokens,
        total_tokens,
        analyzed_at: now,
        created_at: now,
        updated_at: now,
    })
}

fn extract_usage(body: &[u8]) -> (Option<i64>, Option<i64>, Option<i64>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None, None);
    };
    let usage = value.get("usage");
    let token = |key: &str| {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(|value| value.as_i64())
    };
    (
        token("prompt_tokens"),
        token("completion_tokens"),
        token("total_tokens"),
    )
}

/// Extract the assistant content from an OpenAI-compatible chat completion
/// envelope. The schema validator must see the model's inner JSON, never the
/// transport wrapper (`choices`/`usage`), otherwise every real response fails.
/// Some local OpenAI-compatible servers return the schema object directly, so
/// a body that is already a JSON object is passed through unchanged.
fn extract_analysis_payload(body: &[u8]) -> Result<Vec<u8>, AiCommandError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        super::command_error(
            AiErrorKind::Provider,
            AiErrorCode::InvalidJson,
            "The AI provider returned invalid JSON.",
            false,
        )
    })?;

    if let Some(content) = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
    {
        return Ok(content.as_bytes().to_vec());
    }

    // Direct-schema responses (some local OpenAI-compatible servers) pass the
    // body through; the schema validator still enforces all field rules.
    if value.is_object() {
        return Ok(body.to_vec());
    }

    Err(super::command_error(
        AiErrorKind::Provider,
        AiErrorCode::SchemaValidation,
        "The AI provider response does not match analysis schema v1.",
        false,
    ))
}

/// Frozen backoff: first retry after 2s, second after 4s.
fn backoff_seconds(attempt_number: i64) -> i64 {
    if attempt_number <= 1 {
        2
    } else {
        4
    }
}

fn error_code_name(code: AiErrorCode) -> String {
    super::preview::error_code_name(code)
}

fn storage_error(operation: &str) -> AiCommandError {
    super::command_error(
        AiErrorKind::Storage,
        AiErrorCode::Db,
        format!("Unable to {operation}."),
        true,
    )
}

fn internal_error(message: &str) -> AiCommandError {
    super::command_error(AiErrorKind::Internal, AiErrorCode::Internal, message, false)
}

fn repo_result<T>(result: anyhow::Result<T>) -> Result<T, AiCommandError> {
    result.map_err(|_error| {
        // Never forward dependency text: SQLite errors can echo constraint
        // values, and the contract requires sanitized error surfaces.
        super::command_error(
            AiErrorKind::Storage,
            AiErrorCode::Db,
            "AI analysis database operation failed.",
            true,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ai::document::CENTRAL_ROOT_LOCK;
    use crate::core::ai::types::{AiBatchStatus, AiJobStatus, AiTargetKind};
    use sha2::Digest;
    use tempfile::tempdir;

    #[test]
    fn backoff_follows_the_frozen_two_four_seconds_sequence() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(3), 4);
    }

    #[test]
    fn usage_extraction_tolerates_absent_usage_object() {
        assert_eq!(extract_usage(b"{}"), (None, None, None));
        assert_eq!(
            extract_usage(
                br#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#
            ),
            (Some(10), Some(5), Some(15))
        );
    }

    #[test]
    fn payload_extraction_unwraps_openai_envelope_and_passes_direct_schema() {
        let inner = serde_json::json!({
            "one_line": "Summary",
            "what_it_does": "Explains",
            "when_to_use": [],
            "how_to_use": [],
            "example_prompts": [],
            "requirements": [],
            "not_for": [],
            "warnings": []
        });
        let envelope = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": inner.to_string()},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let payload = extract_analysis_payload(&serde_json::to_vec(&envelope).unwrap()).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed["one_line"], "Summary");

        // Direct-schema responses pass through unchanged.
        let direct = serde_json::to_vec(&inner).unwrap();
        assert_eq!(extract_analysis_payload(&direct).unwrap(), direct);
    }

    #[test]
    fn payload_extraction_rejects_missing_content_and_invalid_json() {
        let envelope = serde_json::json!({"choices": [], "usage": {}});
        // An object body without message.content is passed through as a direct
        // schema candidate; the schema validator (not extraction) must reject
        // its missing business fields.
        let payload = extract_analysis_payload(&serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert_eq!(
            validate_ai_analysis_result_v1(&payload).unwrap_err().code,
            AiErrorCode::SchemaValidation
        );
        // A non-object body is rejected by extraction itself.
        assert_eq!(
            extract_analysis_payload(b"[1,2,3]").unwrap_err().code,
            AiErrorCode::SchemaValidation
        );
        assert_eq!(
            extract_analysis_payload(b"{not-json").unwrap_err().code,
            AiErrorCode::InvalidJson
        );
    }

    fn claim_batch(id: &str) -> super::super::types::AiBatchRecord {
        super::super::types::AiBatchRecord {
            id: id.into(),
            status: AiBatchStatus::Queued,
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1/".into(),
            model: "local-model".into(),
            output_language: "en".into(),
            prompt_version: "ai-analysis-prompt-v1".into(),
            schema_version: 1,
            timeout_seconds: 60,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
            estimated_input_tokens: 10,
            estimated_output_tokens: 5,
            estimated_cost_micros: None,
            estimated_max_retry_cost_micros: None,
            total_targets: 1,
            valid_documents: 1,
            missing_documents: 0,
            unreadable_documents: 0,
            skipped_targets: 0,
            pause_requested: false,
            cancel_requested: false,
            confirmed_at: 1,
            created_at: 1,
            updated_at: 1,
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn content_change_after_preview_fails_without_network() {
        let _guard = CENTRAL_ROOT_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let store = Arc::new(SkillStore::new(&directory.path().join("service.db")).unwrap());
        let skill_root = directory.path().join("skills");
        let skill_dir = skill_root.join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "version one").unwrap();
        crate::core::central_repo::set_runtime_skills_dir_override(Some(skill_root.clone()));
        store
            .insert_skill(&crate::core::skill_store::SkillRecord {
                id: "svc-managed".into(),
                name: "Demo".into(),
                description: None,
                source_type: "git".into(),
                source_ref: None,
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: skill_dir.to_string_lossy().into_owned(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ready".into(),
                update_status: "in_sync".into(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();

        let repository = super::super::repository::AiRepository::new(&store);
        let batch = claim_batch("svc-batch");
        let now = now_millis();
        let job = super::super::types::AiJobRecord {
            id: "svc-job".into(),
            batch_id: batch.id.clone(),
            ordinal: 0,
            target_kind: AiTargetKind::Managed,
            target_key: "[\"svc-managed\"]".into(),
            target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"svc-managed\"}".into(),
            skill_name: "Demo".into(),
            // Previewed hash of "version one", but the document is changed
            // before the runner executes.
            expected_source_hash: hex::encode(sha2::Sha256::digest(b"version one")),
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
        };
        repository
            .insert_batch_with_jobs(&batch, &[job.clone()])
            .unwrap();

        // The document changes after preview: "version two".
        std::fs::write(skill_dir.join("SKILL.md"), "version two").unwrap();

        let claimed = repository
            .claim_next_job(now_millis())
            .unwrap()
            .expect("job should be claimable");
        let state = Arc::new(super::super::runner::AiRuntimeState::new());
        process_job(store.clone(), state, claimed).await;

        let status: String = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT status FROM ai_analysis_jobs WHERE id='svc-job'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(status, "failed");
        let error_code: String = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT error_code FROM ai_analysis_jobs WHERE id='svc-job'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(error_code, "content_changed");
        crate::core::central_repo::set_runtime_skills_dir_override(None);
    }

    #[tokio::test]
    async fn full_analysis_closed_loop_accepts_openai_chat_envelope() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = CENTRAL_ROOT_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let store = Arc::new(SkillStore::new(&directory.path().join("service.db")).unwrap());
        let skill_root = directory.path().join("skills");
        let skill_dir = skill_root.join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "version one").unwrap();
        crate::core::central_repo::set_runtime_skills_dir_override(Some(skill_root.clone()));
        store
            .insert_skill(&crate::core::skill_store::SkillRecord {
                id: "svc-envelope".into(),
                name: "Demo".into(),
                description: None,
                source_type: "git".into(),
                source_ref: None,
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: skill_dir.to_string_lossy().into_owned(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ready".into(),
                update_status: "in_sync".into(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();

        // Realistic OpenAI-compatible chat completion response: the schema
        // object lives inside choices[0].message.content, plus usage tokens.
        let inner = serde_json::json!({
            "one_line": "Summarizes the skill.",
            "what_it_does": "Explains capabilities.",
            "when_to_use": [],
            "how_to_use": [],
            "example_prompts": [],
            "requirements": [],
            "not_for": [],
            "warnings": []
        });
        let envelope = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": inner.to_string()},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}
        });
        let body = serde_json::to_vec(&envelope).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 8192];
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let base_url = format!("http://{address}/v1/");

        crate::core::ai::config::save_config(
            &store,
            &super::super::types::AiConfigInput {
                provider: "ollama".into(),
                base_url: base_url.clone(),
                model: "local-model".into(),
                output_language: "en".into(),
                timeout_seconds: 30,
                concurrency: 1,
                log_retention_days: 30,
                input_price_micros_per_million: None,
                output_price_micros_per_million: None,
            },
        )
        .unwrap();

        let repository = super::super::repository::AiRepository::new(&store);
        let mut batch = claim_batch("svc-envelope-batch");
        batch.base_url = base_url;
        let now = now_millis();
        let job = super::super::types::AiJobRecord {
            id: "svc-envelope-job".into(),
            batch_id: batch.id.clone(),
            ordinal: 0,
            target_kind: AiTargetKind::Managed,
            target_key: "[\"svc-envelope\"]".into(),
            target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"svc-envelope\"}".into(),
            skill_name: "Demo".into(),
            expected_source_hash: hex::encode(sha2::Sha256::digest(b"version one")),
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
        };
        repository
            .insert_batch_with_jobs(&batch, &[job.clone()])
            .unwrap();

        let claimed = repository
            .claim_next_job(now_millis())
            .unwrap()
            .expect("job should be claimable");
        let state = Arc::new(super::super::runner::AiRuntimeState::new());
        process_job(store.clone(), state, claimed).await;

        let status: String = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT status FROM ai_analysis_jobs WHERE id='svc-envelope-job'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(status, "succeeded");

        let (one_line, input_tokens, output_tokens, total_tokens): (
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT one_line, input_tokens, output_tokens, total_tokens
                             FROM skill_ai_analyses WHERE target_key='[\"svc-envelope\"]'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(one_line, "Summarizes the skill.");
        assert_eq!(input_tokens, Some(12));
        assert_eq!(output_tokens, Some(8));
        assert_eq!(total_tokens, Some(20));

        crate::core::central_repo::set_runtime_skills_dir_override(None);
    }
}
