use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::logs::SanitizedAiLogRecord;
use super::types::{
    AiAnalysisRecord, AiBatchRecord, AiBatchStatus, AiJobRecord, AiJobStatus, AiLogEventKind,
    AiLogRecord, AiTargetKind, AiTargetRef,
};
use crate::core::skill_store::SkillStore;

/// Canonical identity helpers shared by every AI table lookup. The key is a
/// whitespace-free JSON array per the frozen contract so two frontends can
/// never generate the same target with different string layouts.
pub(crate) fn canonical_target(target: &AiTargetRef) -> (AiTargetKind, String, String) {
    let (kind, key) = match target {
        AiTargetRef::Managed { skill_id } => {
            (AiTargetKind::Managed, serde_json::to_string(&[skill_id]))
        }
        AiTargetRef::GlobalLocal {
            agent_key,
            relative_path,
        } => (
            AiTargetKind::GlobalLocal,
            serde_json::to_string(&[agent_key, relative_path]),
        ),
        AiTargetRef::ProjectLocal {
            project_id,
            agent_key,
            relative_path,
        } => (
            AiTargetKind::ProjectLocal,
            serde_json::to_string(&[project_id, agent_key, relative_path]),
        ),
    };
    let key = key.expect("target key serialization cannot fail for strings");
    let payload = serde_json::to_string(target)
        .expect("target payload serialization cannot fail for strings");
    (kind, key, payload)
}

pub(crate) fn target_ref_from_payload(payload: &str) -> Result<AiTargetRef> {
    serde_json::from_str(payload).context("stored AI target payload is invalid")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub batch: AiBatchRecord,
    pub job: AiJobRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptReservation {
    Reserved { attempt_number: i64 },
    Cancelled,
    NoBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    Failed,
    RetryScheduled(Option<i64>),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    Succeeded,
    Cancelled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    pub interrupted: i64,
    pub requeued: i64,
    pub cancelled_jobs: i64,
    pub cancelled_batches: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelJobOutcome {
    Cancelled,
    RunningCancelled,
    InvalidState,
}

/// Raw per-target state used by detail/summary status computation.
#[derive(Debug, Default)]
pub struct TargetState {
    pub analysis: Option<AiAnalysisRecord>,
    pub active_job: Option<AiJobRecord>,
    pub latest_failed_job: Option<AiJobRecord>,
    pub active_batch_paused: bool,
}

/// The repository is deliberately tied to SkillStore so AI data can never be
/// written through a second connection that skipped application migrations.
pub struct AiRepository<'a> {
    store: &'a SkillStore,
}

impl<'a> AiRepository<'a> {
    pub fn new(store: &'a SkillStore) -> Self {
        Self { store }
    }

    pub fn insert_analysis(&self, record: &AiAnalysisRecord) -> Result<()> {
        self.store.with_ai_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO skill_ai_analyses (
                    id,target_kind,target_key,target_payload_json,skill_name,source_hash,
                    schema_version,prompt_version,output_language,one_line,result_json,
                    provider,model,input_tokens,output_tokens,total_tokens,
                    analyzed_at,created_at,updated_at
                 ) VALUES (
                    ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19
                 )",
                params![
                    record.id,
                    record.target_kind.as_str(),
                    record.target_key,
                    record.target_payload_json,
                    record.skill_name,
                    record.source_hash,
                    record.schema_version,
                    record.prompt_version,
                    record.output_language,
                    record.one_line,
                    record.result_json,
                    record.provider,
                    record.model,
                    record.input_tokens,
                    record.output_tokens,
                    record.total_tokens,
                    record.analyzed_at,
                    record.created_at,
                    record.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn insert_batch(&self, batch: &AiBatchRecord) -> Result<()> {
        self.store
            .with_ai_transaction(|transaction| insert_batch(transaction, batch))
    }

    pub fn insert_job(&self, job: &AiJobRecord) -> Result<()> {
        self.store
            .with_ai_transaction(|transaction| insert_job(transaction, job))
    }

    /// Batch creation is all-or-nothing because a confirmed cost snapshot
    /// without its complete job set would misrepresent both progress and spend.
    pub fn insert_batch_with_jobs(
        &self,
        batch: &AiBatchRecord,
        jobs: &[AiJobRecord],
    ) -> Result<()> {
        if jobs.iter().any(|job| job.batch_id != batch.id) {
            bail!("AI job batch_id does not match the batch being inserted");
        }

        self.store.with_ai_transaction(|transaction| {
            insert_batch(transaction, batch)?;
            for job in jobs {
                insert_job(transaction, job)?;
            }
            Ok(())
        })
    }

    pub(super) fn insert_log(&self, sanitized: &SanitizedAiLogRecord) -> Result<()> {
        self.store
            .with_ai_transaction(|transaction| insert_log(transaction, sanitized))
    }

    /// Pick the next claimable job in the frozen order. Expired `retry_wait`
    /// jobs are promoted to `queued` in the same transaction, and the batch
    /// moves `queued -> running` when its first job is claimed.
    pub fn claim_next_job(&self, now: i64) -> Result<Option<ClaimedJob>> {
        self.store.with_ai_transaction(|transaction| {
            transaction.execute(
                "UPDATE ai_analysis_jobs SET status='queued', updated_at=?1
                 WHERE status='retry_wait' AND next_retry_at IS NOT NULL AND next_retry_at <= ?1",
                params![now],
            )?;

            let row: Option<(String, String)> = transaction
                .query_row(
                    "SELECT j.id, j.batch_id
                     FROM ai_analysis_jobs j
                     JOIN ai_analysis_batches b ON b.id = j.batch_id
                     WHERE j.status='queued'
                       AND b.status IN ('queued','running')
                       AND b.pause_requested=0 AND b.cancel_requested=0
                     ORDER BY j.priority DESC, j.created_at ASC, j.batch_id ASC, j.ordinal ASC, j.id ASC
                     LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((job_id, batch_id)) = row else {
                return Ok(None);
            };

            // First claim turns the batch running; a concurrently cancelled
            // batch is skipped by the WHERE clause.
            transaction.execute(
                "UPDATE ai_analysis_batches SET status='running', updated_at=?1
                 WHERE id=?2 AND status='queued' AND cancel_requested=0 AND pause_requested=0",
                params![now, batch_id],
            )?;
            transaction.execute(
                "UPDATE ai_analysis_jobs SET status='running', started_at=?1, updated_at=?1
                 WHERE id=?2 AND status='queued' AND cancel_requested=0",
                params![now, job_id],
            )?;

            let batch = load_batch(&*transaction, &batch_id)?.context("claimed batch missing")?;
            let job = load_job(&*transaction, &job_id)?.context("claimed job missing")?;
            Ok(Some(ClaimedJob { batch, job }))
        })
    }

    /// Pre-commit one HTTP attempt: verify budget and cancel flags, increment
    /// the attempt counter (and the one-shot correction flag when needed), and
    /// persist the `request_started` log in the same transaction. Only after
    /// commit may the caller send the network request.
    pub(super) fn reserve_http_attempt(
        &self,
        job_id: &str,
        correction: bool,
        now: i64,
        log: SanitizedAiLogRecord,
    ) -> Result<AttemptReservation> {
        self.store.with_ai_transaction(|transaction| {
            let state: (String, i64, i64, i64) = transaction
                .query_row(
                    "SELECT j.status, j.attempt_count, j.cancel_requested, b.cancel_requested
                     FROM ai_analysis_jobs j JOIN ai_analysis_batches b ON b.id = j.batch_id
                     WHERE j.id = ?1",
                    params![job_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
                .context("reserved job no longer exists")?;
            if state.0 != "running" {
                return Ok(AttemptReservation::Cancelled);
            }
            if state.2 != 0 || state.3 != 0 {
                return Ok(AttemptReservation::Cancelled);
            }
            if state.1 >= 3 {
                return Ok(AttemptReservation::NoBudget);
            }
            let attempt_number = state.1 + 1;
            transaction.execute(
                "UPDATE ai_analysis_jobs
                 SET attempt_count=?1, correction_attempted=CASE WHEN ?2 THEN 1 ELSE correction_attempted END,
                     updated_at=?3
                 WHERE id=?4",
                params![attempt_number, correction, now, job_id],
            )?;
            insert_log(transaction, &log)?;
            Ok(AttemptReservation::Reserved { attempt_number })
        })
    }

    /// Commit a successful analysis together with its log and job terminal
    /// state. The cancel flags are re-read inside this transaction so the
    /// SQLite write lock, not the network response, is the linearization point.
    pub(super) fn complete_success(
        &self,
        job_id: &str,
        analysis: &AiAnalysisRecord,
        log: SanitizedAiLogRecord,
        now: i64,
    ) -> Result<CompleteOutcome> {
        self.store.with_ai_transaction(|transaction| {
            let state: Option<(String, String, i64, i64)> = transaction
                .query_row(
                    "SELECT j.batch_id, j.status, j.cancel_requested, b.cancel_requested
                     FROM ai_analysis_jobs j JOIN ai_analysis_batches b ON b.id = j.batch_id
                     WHERE j.id = ?1",
                    params![job_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((batch_id, status, job_cancel, batch_cancel)) = state else {
                bail!("AI job {job_id} no longer exists");
            };
            if status != "running" {
                return Ok(CompleteOutcome::Cancelled);
            }

            if job_cancel != 0 || batch_cancel != 0 {
                transaction.execute(
                    "UPDATE ai_analysis_jobs SET status='cancelled', finished_at=?1, updated_at=?1
                     WHERE id=?2 AND status='running'",
                    params![now, job_id],
                )?;
                let cancelled_log = cancelled_log(&log, now);
                insert_log(transaction, &cancelled_log)?;
                finalize_batch(transaction, &batch_id, now)?;
                return Ok(CompleteOutcome::Cancelled);
            }

            // One result per target: replace any previous success so the table
            // never accumulates stale rows for the same identity.
            transaction.execute(
                "DELETE FROM skill_ai_analyses WHERE target_kind=?1 AND target_key=?2",
                params![analysis.target_kind.as_str(), analysis.target_key],
            )?;
            transaction.execute(
                "INSERT INTO skill_ai_analyses (
                    id,target_kind,target_key,target_payload_json,skill_name,source_hash,
                    schema_version,prompt_version,output_language,one_line,result_json,
                    provider,model,input_tokens,output_tokens,total_tokens,
                    analyzed_at,created_at,updated_at
                 ) VALUES (
                    ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19
                 )",
                params![
                    analysis.id,
                    analysis.target_kind.as_str(),
                    analysis.target_key,
                    analysis.target_payload_json,
                    analysis.skill_name,
                    analysis.source_hash,
                    analysis.schema_version,
                    analysis.prompt_version,
                    analysis.output_language,
                    analysis.one_line,
                    analysis.result_json,
                    analysis.provider,
                    analysis.model,
                    analysis.input_tokens,
                    analysis.output_tokens,
                    analysis.total_tokens,
                    analysis.analyzed_at,
                    analysis.created_at,
                    analysis.updated_at,
                ],
            )?;
            transaction.execute(
                "UPDATE ai_analysis_jobs SET status='succeeded', finished_at=?1, updated_at=?1
                 WHERE id=?2 AND status='running'",
                params![now, job_id],
            )?;
            insert_log(transaction, &log)?;
            finalize_batch(transaction, &batch_id, now)?;
            Ok(CompleteOutcome::Succeeded)
        })
    }

    /// Persist a failure. `retry_wait_seconds` schedules a retry (the counter
    /// was already consumed by `reserve_http_attempt`); `None` or exhausted
    /// budget produces the terminal `failed` state.
    pub(super) fn fail_job(
        &self,
        job_id: &str,
        error_code: &str,
        error_message: &str,
        retry_wait_seconds: Option<i64>,
        log: SanitizedAiLogRecord,
        now: i64,
    ) -> Result<FailOutcome> {
        self.store.with_ai_transaction(|transaction| {
            let state: Option<(String, String, i64, i64, i64)> = transaction
                .query_row(
                    "SELECT j.batch_id, j.status, j.attempt_count, j.cancel_requested, b.cancel_requested
                     FROM ai_analysis_jobs j JOIN ai_analysis_batches b ON b.id = j.batch_id
                     WHERE j.id = ?1",
                    params![job_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((batch_id, status, attempt_count, job_cancel, batch_cancel)) = state else {
                bail!("AI job {job_id} no longer exists");
            };
            if status != "running" {
                return Ok(FailOutcome::Cancelled);
            }
            if job_cancel != 0 || batch_cancel != 0 {
                transaction.execute(
                    "UPDATE ai_analysis_jobs SET status='cancelled', finished_at=?1, updated_at=?1
                     WHERE id=?2 AND status='running'",
                    params![now, job_id],
                )?;
                let cancelled_log = cancelled_log(&log, now);
                insert_log(transaction, &cancelled_log)?;
                finalize_batch(transaction, &batch_id, now)?;
                return Ok(FailOutcome::Cancelled);
            }

            let can_retry = retry_wait_seconds.is_some() && attempt_count < 3;
            if can_retry {
                let wait = retry_wait_seconds.unwrap_or(0);
                let next_retry_at = now.saturating_add(wait.saturating_mul(1_000));
                transaction.execute(
                    "UPDATE ai_analysis_jobs
                     SET status='retry_wait', next_retry_at=?1, error_code=?2, error_message=?3,
                         updated_at=?4
                     WHERE id=?5",
                    params![next_retry_at, error_code, error_message, now, job_id],
                )?;
                insert_log(transaction, &log)?;
                Ok(FailOutcome::RetryScheduled(Some(next_retry_at)))
            } else {
                transaction.execute(
                    "UPDATE ai_analysis_jobs
                     SET status='failed', finished_at=?1, error_code=?2, error_message=?3,
                         updated_at=?1
                     WHERE id=?4",
                    params![now, error_code, error_message, job_id],
                )?;
                insert_log(transaction, &log)?;
                finalize_batch(transaction, &batch_id, now)?;
                Ok(FailOutcome::Failed)
            }
        })
    }

    /// Startup recovery in the frozen order: interrupted running jobs, cancel
    /// propagation, pause preservation, then requeue with attempt counters
    /// intact.
    pub fn recover_on_startup(&self, now: i64) -> Result<RecoverySummary> {
        self.store.with_ai_transaction(|transaction| {
            let interrupted = transaction.execute(
                "UPDATE ai_analysis_jobs SET status='interrupted', updated_at=?1 WHERE status='running'",
                params![now],
            )?;
            if interrupted > 0 {
                let recovery_log = crate::core::ai::logs::sanitized_record(
                    AiLogRecord {
                        id: uuid::Uuid::new_v4().to_string(),
                        event_kind: AiLogEventKind::Recovery,
                        job_id: None,
                        batch_id: None,
                        target_kind: None,
                        target_key: None,
                        target_payload_json: None,
                        skill_name: None,
                        request_system_prompt: None,
                        request_user_prompt: None,
                        raw_response: None,
                        http_status: None,
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                        duration_ms: None,
                        error_code: Some("recovery".into()),
                        error_message: Some(
                            "application restarted; running jobs were interrupted".into(),
                        ),
                        created_at: now,
                    },
                    None,
                );
                insert_log(transaction, &recovery_log)?;
            }

            let cancelled_jobs = transaction.execute(
                "UPDATE ai_analysis_jobs SET status='cancelled', finished_at=?1, updated_at=?1
                 WHERE cancel_requested=1 AND status IN ('queued','retry_wait','interrupted')",
                params![now],
            )?;
            let cancelled_batches = transaction.execute(
                "UPDATE ai_analysis_batches SET status='cancelled', pause_requested=0,
                        finished_at=?1, updated_at=?1
                 WHERE cancel_requested=1 AND status IN ('queued','running','paused','cancelling')
                   AND NOT EXISTS (
                     SELECT 1 FROM ai_analysis_jobs j
                     WHERE j.batch_id = ai_analysis_batches.id
                       AND j.status NOT IN ('succeeded','failed','cancelled')
                   )",
                params![now],
            )?;
            let requeued = transaction.execute(
                "UPDATE ai_analysis_jobs SET status='queued', updated_at=?1
                 WHERE status='interrupted'
                   AND batch_id IN (
                     SELECT id FROM ai_analysis_batches
                     WHERE pause_requested=0 AND cancel_requested=0
                   )",
                params![now],
            )?;
            transaction.execute(
                "UPDATE ai_analysis_batches SET status='queued', updated_at=?1
                 WHERE status IN ('running','paused')
                   AND pause_requested=0 AND cancel_requested=0
                   AND EXISTS (
                     SELECT 1 FROM ai_analysis_jobs j
                     WHERE j.batch_id = ai_analysis_batches.id
                       AND j.status NOT IN ('succeeded','failed','cancelled')
                   )",
                params![now],
            )?;
            Ok(RecoverySummary {
                interrupted: i64::try_from(interrupted).unwrap_or(i64::MAX),
                requeued: i64::try_from(requeued).unwrap_or(i64::MAX),
                cancelled_jobs: i64::try_from(cancelled_jobs).unwrap_or(i64::MAX),
                cancelled_batches: i64::try_from(cancelled_batches).unwrap_or(i64::MAX),
            })
        })
    }

    /// Load per-target analysis, active job, and latest failed job so status
    /// computation never mixes result and error provenance.
    pub fn get_target_state(&self, target: &AiTargetRef) -> Result<TargetState> {
        let (kind, key, _) = canonical_target(target);
        self.get_target_state_by_key(&kind, &key)
    }

    pub fn get_target_state_by_key(&self, kind: &AiTargetKind, key: &str) -> Result<TargetState> {
        self.store.with_ai_connection(|connection| {
            let analysis = connection
                .query_row(
                    "SELECT id,target_kind,target_key,target_payload_json,skill_name,source_hash,
                            schema_version,prompt_version,output_language,one_line,result_json,
                            provider,model,input_tokens,output_tokens,total_tokens,
                            analyzed_at,created_at,updated_at
                     FROM skill_ai_analyses WHERE target_kind=?1 AND target_key=?2",
                    params![kind.as_str(), key],
                    map_analysis_row,
                )
                .optional()?;
            let active_job = connection
                .query_row(
                    "SELECT id,batch_id,ordinal,target_kind,target_key,target_payload_json,skill_name,
                            expected_source_hash,status,priority,attempt_count,manual_retry_count,
                            correction_attempted,cancel_requested,next_retry_at,error_code,error_message,
                            created_at,updated_at,started_at,finished_at
                     FROM ai_analysis_jobs
                     WHERE target_kind=?1 AND target_key=?2
                       AND status IN ('queued','running','retry_wait','interrupted')
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    params![kind.as_str(), key],
                    map_job_row,
                )
                .optional()?;
            let active_batch_paused = match &active_job {
                Some(job) => connection
                    .query_row(
                        "SELECT status='paused' FROM ai_analysis_batches WHERE id=?1",
                        params![job.batch_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(false),
                None => false,
            };
            let latest_failed_job = connection
                .query_row(
                    "SELECT id,batch_id,ordinal,target_kind,target_key,target_payload_json,skill_name,
                            expected_source_hash,status,priority,attempt_count,manual_retry_count,
                            correction_attempted,cancel_requested,next_retry_at,error_code,error_message,
                            created_at,updated_at,started_at,finished_at
                     FROM ai_analysis_jobs
                     WHERE target_kind=?1 AND target_key=?2 AND status='failed'
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    params![kind.as_str(), key],
                    map_job_row,
                )
                .optional()?;
            Ok(TargetState {
                analysis,
                active_job,
                latest_failed_job,
                active_batch_paused,
            })
        })
    }

    pub fn load_batch_dto(&self, batch_id: &str) -> Result<Option<AiBatchRecord>> {
        self.store
            .with_ai_connection(|connection| load_batch(connection, batch_id))
    }

    pub fn batch_job_counts(&self, batch_id: &str) -> Result<(i64, i64, i64, i64, i64, i64)> {
        self.store.with_ai_connection(|connection| {
            connection
                .query_row(
                    "SELECT
                        SUM(CASE WHEN status='queued' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status='running' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status='retry_wait' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status='interrupted' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status='succeeded' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END)
                     FROM ai_analysis_jobs WHERE batch_id=?1",
                    params![batch_id],
                    |row| {
                        let zero = |value: Option<i64>| value.unwrap_or(0);
                        Ok((
                            zero(row.get(0)?),
                            zero(row.get(1)?),
                            zero(row.get(2)?),
                            zero(row.get(3)?),
                            zero(row.get(4)?),
                            zero(row.get(5)?),
                        ))
                    },
                )
                .map_err(anyhow::Error::from)
        })
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<AiJobRecord>> {
        self.store
            .with_ai_connection(|connection| load_job(connection, job_id))
    }

    pub fn list_batches(
        &self,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<AiBatchRecord>, Option<String>)> {
        self.store.with_ai_connection(|connection| {
            let (cursor_created, cursor_id) = parse_cursor(cursor)?;
            let page_size = i64::from(limit) + 1;
            let mut sql = String::from(
                "SELECT id,status,provider,base_url,model,output_language,prompt_version,schema_version,
                    timeout_seconds,input_price_micros_per_million,output_price_micros_per_million,
                    estimated_input_tokens,estimated_output_tokens,estimated_cost_micros,
                    estimated_max_retry_cost_micros,total_targets,valid_documents,missing_documents,
                    unreadable_documents,skipped_targets,pause_requested,cancel_requested,
                    confirmed_at,created_at,updated_at,finished_at
                 FROM ai_analysis_batches WHERE 1=1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(status) = status {
                sql.push_str(" AND status = ?");
                params.push(Box::new(status.to_string()));
            }
            if let (Some(created_at), Some(id)) = (cursor_created, cursor_id) {
                sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
                params.push(Box::new(created_at));
                params.push(Box::new(created_at));
                params.push(Box::new(id));
            }
            sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
            params.push(Box::new(page_size));

            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(params.iter().map(|value| value.as_ref())),
                map_batch_row,
            )?;
            let mut batches = Vec::new();
            for row in rows {
                batches.push(row?);
            }
            let next_cursor = if batches.len() as i64 > i64::from(limit) {
                batches.pop().expect("page has a sentinel row");
                // The sentinel proves another page exists; the cursor itself
                // comes from the final returned row of this page.
                let final_row = batches.last().expect("page has at least one row");
                Some(encode_cursor(final_row.created_at, &final_row.id))
            } else {
                None
            };
            Ok((batches, next_cursor))
        })
    }

    pub fn list_jobs(
        &self,
        batch_id: Option<&str>,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<AiJobRecord>, Option<String>)> {
        self.store.with_ai_connection(|connection| {
            let (cursor_created, cursor_id) = parse_cursor(cursor)?;
            let page_size = i64::from(limit) + 1;
            let mut sql = String::from(
                "SELECT id,batch_id,ordinal,target_kind,target_key,target_payload_json,skill_name,
                    expected_source_hash,status,priority,attempt_count,manual_retry_count,
                    correction_attempted,cancel_requested,next_retry_at,error_code,error_message,
                    created_at,updated_at,started_at,finished_at
                 FROM ai_analysis_jobs WHERE 1=1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(batch_id) = batch_id {
                sql.push_str(" AND batch_id = ?");
                params.push(Box::new(batch_id.to_string()));
            }
            if let Some(status) = status {
                sql.push_str(" AND status = ?");
                params.push(Box::new(status.to_string()));
            }
            if let (Some(created_at), Some(id)) = (cursor_created, cursor_id) {
                sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
                params.push(Box::new(created_at));
                params.push(Box::new(created_at));
                params.push(Box::new(id));
            }
            sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
            params.push(Box::new(page_size));

            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(params.iter().map(|value| value.as_ref())),
                map_job_row,
            )?;
            let mut jobs = Vec::new();
            for row in rows {
                jobs.push(row?);
            }
            let next_cursor = if jobs.len() as i64 > i64::from(limit) {
                jobs.pop().expect("page has a sentinel row");
                let final_row = jobs.last().expect("page has at least one row");
                Some(encode_cursor(final_row.created_at, &final_row.id))
            } else {
                None
            };
            Ok((jobs, next_cursor))
        })
    }

    pub fn pause_batch(&self, batch_id: &str, now: i64) -> Result<Option<AiBatchRecord>> {
        self.store.with_ai_transaction(|transaction| {
            let changed = transaction.execute(
                "UPDATE ai_analysis_batches
                 SET status='paused', pause_requested=1, updated_at=?1
                 WHERE id=?2 AND status IN ('queued','running') AND cancel_requested=0",
                params![now, batch_id],
            )?;
            if changed == 0 {
                return load_batch(transaction, batch_id);
            }
            load_batch(transaction, batch_id)
        })
    }

    pub fn resume_batch(&self, batch_id: &str, now: i64) -> Result<Option<AiBatchRecord>> {
        self.store.with_ai_transaction(|transaction| {
            let current = load_batch(transaction, batch_id)?;
            let Some(batch) = current else {
                return Ok(None);
            };
            if batch.status != AiBatchStatus::Paused || batch.cancel_requested {
                return Ok(Some(batch));
            }
            let remaining: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM ai_analysis_jobs
                 WHERE batch_id=?1 AND status NOT IN ('succeeded','failed','cancelled')",
                params![batch_id],
                |row| row.get(0),
            )?;
            if remaining == 0 {
                transaction.execute(
                    "UPDATE ai_analysis_batches
                     SET status='completed', pause_requested=0, cancel_requested=0,
                         finished_at=?1, updated_at=?1
                     WHERE id=?2",
                    params![now, batch_id],
                )?;
            } else {
                transaction.execute(
                    "UPDATE ai_analysis_batches
                     SET status='queued', pause_requested=0, updated_at=?1
                     WHERE id=?2",
                    params![now, batch_id],
                )?;
            }
            load_batch(transaction, batch_id)
        })
    }

    /// Cancel a batch: persist intent, move non-running jobs to cancelled, and
    /// return running job ids so the command layer can request their cancel.
    pub fn cancel_batch(
        &self,
        batch_id: &str,
        now: i64,
    ) -> Result<(Option<AiBatchRecord>, Vec<String>)> {
        self.store.with_ai_transaction(|transaction| {
            let running: Vec<String> = {
                let mut statement = transaction.prepare(
                    "SELECT id FROM ai_analysis_jobs
                     WHERE batch_id=?1 AND status='running'",
                )?;
                let rows = statement.query_map(params![batch_id], |row| row.get(0))?;
                rows.filter_map(|row| row.ok()).collect()
            };
            let changed = transaction.execute(
                "UPDATE ai_analysis_batches
                 SET status='cancelling', cancel_requested=1, updated_at=?1
                 WHERE id=?2 AND status IN ('queued','running','paused') AND cancel_requested=0",
                params![now, batch_id],
            )?;
            if changed == 0 {
                return Ok((load_batch(transaction, batch_id)?, running));
            }
            transaction.execute(
                "UPDATE ai_analysis_jobs
                 SET status='cancelled', cancel_requested=1, finished_at=?1, updated_at=?1
                 WHERE batch_id=?2 AND status IN ('queued','retry_wait','interrupted')",
                params![now, batch_id],
            )?;
            let remaining: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM ai_analysis_jobs
                 WHERE batch_id=?1 AND status NOT IN ('succeeded','failed','cancelled')",
                params![batch_id],
                |row| row.get(0),
            )?;
            if remaining == 0 {
                transaction.execute(
                    "UPDATE ai_analysis_batches
                     SET status='cancelled', pause_requested=0, cancel_requested=0,
                         finished_at=?1, updated_at=?1
                     WHERE id=?2",
                    params![now, batch_id],
                )?;
            }
            Ok((load_batch(transaction, batch_id)?, running))
        })
    }

    pub fn cancel_job(&self, job_id: &str, now: i64) -> Result<CancelJobOutcome> {
        self.store.with_ai_transaction(|transaction| {
            let current = load_job(transaction, job_id)?;
            let Some(job) = current else {
                return Ok(CancelJobOutcome::InvalidState);
            };
            match job.status {
                AiJobStatus::Cancelled => return Ok(CancelJobOutcome::Cancelled),
                AiJobStatus::Succeeded | AiJobStatus::Failed => {
                    return Ok(CancelJobOutcome::InvalidState);
                }
                AiJobStatus::Running => {
                    transaction.execute(
                        "UPDATE ai_analysis_jobs SET cancel_requested=1, updated_at=?1 WHERE id=?2",
                        params![now, job_id],
                    )?;
                    return Ok(CancelJobOutcome::RunningCancelled);
                }
                AiJobStatus::Queued | AiJobStatus::RetryWait | AiJobStatus::Interrupted => {
                    transaction.execute(
                        "UPDATE ai_analysis_jobs
                         SET status='cancelled', cancel_requested=1, finished_at=?1, updated_at=?1
                         WHERE id=?2",
                        params![now, job_id],
                    )?;
                    finalize_batch(transaction, &job.batch_id, now)?;
                    return Ok(CancelJobOutcome::Cancelled);
                }
            }
        })
    }

    /// Aggregate job/batch counters for the manager page.
    pub fn queue_counts(
        &self,
    ) -> Result<(
        (i64, i64, i64, i64, i64, i64),
        (i64, i64, i64, i64, i64, i64),
    )> {
        self.store.with_ai_connection(|connection| {
            let batch_counts = connection.query_row(
                "SELECT
                    SUM(CASE WHEN status='queued' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='running' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='paused' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='cancelling' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='completed' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='cancelled' THEN 1 ELSE 0 END)
                 FROM ai_analysis_batches",
                [],
                |row| {
                    let zero = |value: Option<i64>| value.unwrap_or(0);
                    Ok((
                        zero(row.get(0)?),
                        zero(row.get(1)?),
                        zero(row.get(2)?),
                        zero(row.get(3)?),
                        zero(row.get(4)?),
                        zero(row.get(5)?),
                    ))
                },
            )?;
            let job_counts = connection.query_row(
                "SELECT
                    SUM(CASE WHEN status='queued' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='running' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='retry_wait' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='interrupted' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='succeeded' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END)
                 FROM ai_analysis_jobs",
                [],
                |row| {
                    let zero = |value: Option<i64>| value.unwrap_or(0);
                    Ok((
                        zero(row.get(0)?),
                        zero(row.get(1)?),
                        zero(row.get(2)?),
                        zero(row.get(3)?),
                        zero(row.get(4)?),
                        zero(row.get(5)?),
                    ))
                },
            )?;
            Ok((batch_counts, job_counts))
        })
    }

    pub fn cancelled_job_count(&self) -> Result<i64> {
        self.store.with_ai_connection(|connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM ai_analysis_jobs WHERE status='cancelled'",
                    [],
                    |row| row.get(0),
                )
                .map_err(anyhow::Error::from)
        })
    }

    pub fn list_logs(
        &self,
        event_kind: Option<&str>,
        error_code: Option<&str>,
        job_id: Option<&str>,
        batch_id: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<AiLogRecord>, Option<String>)> {
        self.store.with_ai_connection(|connection| {
            let (cursor_created, cursor_id) = parse_cursor(cursor)?;
            let page_size = i64::from(limit) + 1;
            let mut sql = String::from(
                "SELECT id,event_kind,job_id,batch_id,target_kind,target_key,target_payload_json,
                    skill_name,request_system_prompt,request_user_prompt,raw_response,http_status,
                    input_tokens,output_tokens,total_tokens,duration_ms,error_code,error_message,created_at
                 FROM ai_analysis_logs WHERE 1=1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(event_kind) = event_kind {
                sql.push_str(" AND event_kind = ?");
                params.push(Box::new(event_kind.to_string()));
            }
            if let Some(error_code) = error_code {
                sql.push_str(" AND error_code = ?");
                params.push(Box::new(error_code.to_string()));
            }
            if let Some(job_id) = job_id {
                sql.push_str(" AND job_id = ?");
                params.push(Box::new(job_id.to_string()));
            }
            if let Some(batch_id) = batch_id {
                sql.push_str(" AND batch_id = ?");
                params.push(Box::new(batch_id.to_string()));
            }
            if let (Some(created_at), Some(id)) = (cursor_created, cursor_id) {
                sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
                params.push(Box::new(created_at));
                params.push(Box::new(created_at));
                params.push(Box::new(id));
            }
            sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
            params.push(Box::new(page_size));

            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(params.iter().map(|value| value.as_ref())),
                map_log_row,
            )?;
            let mut logs = Vec::new();
            for row in rows {
                logs.push(row?);
            }
            let next_cursor = if logs.len() as i64 > i64::from(limit) {
                logs.pop().expect("page has a sentinel row");
                let final_row = logs.last().expect("page has at least one row");
                Some(encode_cursor(final_row.created_at, &final_row.id))
            } else {
                None
            };
            Ok((logs, next_cursor))
        })
    }

    pub fn get_log(&self, log_id: &str) -> Result<Option<AiLogRecord>> {
        self.store.with_ai_connection(|connection| {
            connection
                .query_row(
                    "SELECT id,event_kind,job_id,batch_id,target_kind,target_key,target_payload_json,
                        skill_name,request_system_prompt,request_user_prompt,raw_response,http_status,
                        input_tokens,output_tokens,total_tokens,duration_ms,error_code,error_message,created_at
                     FROM ai_analysis_logs WHERE id=?1",
                    params![log_id],
                    map_log_row,
                )
                .optional()
                .map_err(anyhow::Error::from)
        })
    }
}

fn insert_batch(transaction: &Transaction<'_>, batch: &AiBatchRecord) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO ai_analysis_batches (
                id,status,provider,base_url,model,output_language,prompt_version,schema_version,
                timeout_seconds,input_price_micros_per_million,output_price_micros_per_million,
                estimated_input_tokens,estimated_output_tokens,estimated_cost_micros,
                estimated_max_retry_cost_micros,total_targets,valid_documents,missing_documents,
                unreadable_documents,skipped_targets,pause_requested,cancel_requested,
                confirmed_at,created_at,updated_at,finished_at
             ) VALUES (
                ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26
             )",
            params![
                batch.id,
                batch.status.as_str(),
                batch.provider,
                batch.base_url,
                batch.model,
                batch.output_language,
                batch.prompt_version,
                batch.schema_version,
                batch.timeout_seconds,
                batch.input_price_micros_per_million,
                batch.output_price_micros_per_million,
                batch.estimated_input_tokens,
                batch.estimated_output_tokens,
                batch.estimated_cost_micros,
                batch.estimated_max_retry_cost_micros,
                batch.total_targets,
                batch.valid_documents,
                batch.missing_documents,
                batch.unreadable_documents,
                batch.skipped_targets,
                batch.pause_requested,
                batch.cancel_requested,
                batch.confirmed_at,
                batch.created_at,
                batch.updated_at,
                batch.finished_at,
            ],
        )
        .context("failed to insert AI analysis batch")?;
    Ok(())
}

fn insert_job(transaction: &Transaction<'_>, job: &AiJobRecord) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO ai_analysis_jobs (
                id,batch_id,ordinal,target_kind,target_key,target_payload_json,skill_name,
                expected_source_hash,status,priority,attempt_count,manual_retry_count,
                correction_attempted,cancel_requested,next_retry_at,error_code,error_message,
                created_at,updated_at,started_at,finished_at
             ) VALUES (
                ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21
             )",
            params![
                job.id,
                job.batch_id,
                job.ordinal,
                job.target_kind.as_str(),
                job.target_key,
                job.target_payload_json,
                job.skill_name,
                job.expected_source_hash,
                job.status.as_str(),
                job.priority,
                job.attempt_count,
                job.manual_retry_count,
                job.correction_attempted,
                job.cancel_requested,
                job.next_retry_at,
                job.error_code,
                job.error_message,
                job.created_at,
                job.updated_at,
                job.started_at,
                job.finished_at,
            ],
        )
        .context("failed to insert AI analysis job")?;
    Ok(())
}

fn insert_log(transaction: &Transaction<'_>, sanitized: &SanitizedAiLogRecord) -> Result<()> {
    // The wrapper's private constructor proves every persisted text field
    // crossed the logs module's exact-key and generic redaction gate.
    let log = sanitized.as_record();
    // Logs accept request/response semantics only; the record type has
    // no credential, cookie, or request-header field by construction.
    transaction.execute(
        "INSERT INTO ai_analysis_logs (
            id,event_kind,job_id,batch_id,target_kind,target_key,target_payload_json,
            skill_name,request_system_prompt,request_user_prompt,raw_response,http_status,
            input_tokens,output_tokens,total_tokens,duration_ms,error_code,error_message,created_at
         ) VALUES (
            ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19
         )",
        params![
            log.id,
            log.event_kind.as_str(),
            log.job_id,
            log.batch_id,
            log.target_kind.map(|kind| kind.as_str()),
            log.target_key,
            log.target_payload_json,
            log.skill_name,
            log.request_system_prompt,
            log.request_user_prompt,
            log.raw_response,
            log.http_status,
            log.input_tokens,
            log.output_tokens,
            log.total_tokens,
            log.duration_ms,
            log.error_code,
            log.error_message,
            log.created_at,
        ],
    )?;
    Ok(())
}

/// Build a `cancelled` log reusing the redaction-safe constructor; the
/// semantic log passed by the runner is replaced so terminal state and log
/// kind stay consistent inside the same transaction.
fn cancelled_log(original: &SanitizedAiLogRecord, now: i64) -> SanitizedAiLogRecord {
    let original = original.as_record();
    crate::core::ai::logs::sanitized_record(
        AiLogRecord {
            id: uuid::Uuid::new_v4().to_string(),
            event_kind: AiLogEventKind::Cancelled,
            job_id: original.job_id.clone(),
            batch_id: original.batch_id.clone(),
            target_kind: original.target_kind,
            target_key: original.target_key.clone(),
            target_payload_json: original.target_payload_json.clone(),
            skill_name: original.skill_name.clone(),
            request_system_prompt: None,
            request_user_prompt: None,
            raw_response: None,
            http_status: original.http_status,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            duration_ms: original.duration_ms,
            error_code: Some("cancelled".into()),
            error_message: Some("The AI analysis job was cancelled.".into()),
            created_at: now,
        },
        None,
    )
}

fn load_batch(connection: &Connection, id: &str) -> Result<Option<AiBatchRecord>> {
    Ok(connection
        .query_row(
            "SELECT id,status,provider,base_url,model,output_language,prompt_version,schema_version,
                    timeout_seconds,input_price_micros_per_million,output_price_micros_per_million,
                    estimated_input_tokens,estimated_output_tokens,estimated_cost_micros,
                    estimated_max_retry_cost_micros,total_targets,valid_documents,missing_documents,
                    unreadable_documents,skipped_targets,pause_requested,cancel_requested,
                    confirmed_at,created_at,updated_at,finished_at
             FROM ai_analysis_batches WHERE id=?1",
            params![id],
            |row| {
                let status: String = row.get(1)?;
                Ok(AiBatchRecord {
                    id: row.get(0)?,
                    status: parse_batch_status(&status),
                    provider: row.get(2)?,
                    base_url: row.get(3)?,
                    model: row.get(4)?,
                    output_language: row.get(5)?,
                    prompt_version: row.get(6)?,
                    schema_version: row.get(7)?,
                    timeout_seconds: row.get(8)?,
                    input_price_micros_per_million: row.get(9)?,
                    output_price_micros_per_million: row.get(10)?,
                    estimated_input_tokens: row.get(11)?,
                    estimated_output_tokens: row.get(12)?,
                    estimated_cost_micros: row.get(13)?,
                    estimated_max_retry_cost_micros: row.get(14)?,
                    total_targets: row.get(15)?,
                    valid_documents: row.get(16)?,
                    missing_documents: row.get(17)?,
                    unreadable_documents: row.get(18)?,
                    skipped_targets: row.get(19)?,
                    pause_requested: row.get::<_, i64>(20)? != 0,
                    cancel_requested: row.get::<_, i64>(21)? != 0,
                    confirmed_at: row.get(22)?,
                    created_at: row.get(23)?,
                    updated_at: row.get(24)?,
                    finished_at: row.get(25)?,
                })
            },
        )
        .optional()?)
}

fn map_batch_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiBatchRecord> {
    let status: String = row.get(1)?;
    Ok(AiBatchRecord {
        id: row.get(0)?,
        status: parse_batch_status(&status),
        provider: row.get(2)?,
        base_url: row.get(3)?,
        model: row.get(4)?,
        output_language: row.get(5)?,
        prompt_version: row.get(6)?,
        schema_version: row.get(7)?,
        timeout_seconds: row.get(8)?,
        input_price_micros_per_million: row.get(9)?,
        output_price_micros_per_million: row.get(10)?,
        estimated_input_tokens: row.get(11)?,
        estimated_output_tokens: row.get(12)?,
        estimated_cost_micros: row.get(13)?,
        estimated_max_retry_cost_micros: row.get(14)?,
        total_targets: row.get(15)?,
        valid_documents: row.get(16)?,
        missing_documents: row.get(17)?,
        unreadable_documents: row.get(18)?,
        skipped_targets: row.get(19)?,
        pause_requested: row.get::<_, i64>(20)? != 0,
        cancel_requested: row.get::<_, i64>(21)? != 0,
        confirmed_at: row.get(22)?,
        created_at: row.get(23)?,
        updated_at: row.get(24)?,
        finished_at: row.get(25)?,
    })
}

fn map_log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiLogRecord> {
    let kind: String = row.get(1)?;
    let target_kind: Option<String> = row.get(4)?;
    Ok(AiLogRecord {
        id: row.get(0)?,
        event_kind: match kind.as_str() {
            "request_started" => AiLogEventKind::RequestStarted,
            "response_received" => AiLogEventKind::ResponseReceived,
            "request_failed" => AiLogEventKind::RequestFailed,
            "retry_scheduled" => AiLogEventKind::RetryScheduled,
            "correction_requested" => AiLogEventKind::CorrectionRequested,
            "recovery" => AiLogEventKind::Recovery,
            _ => AiLogEventKind::Cancelled,
        },
        job_id: row.get(2)?,
        batch_id: row.get(3)?,
        target_kind: target_kind.as_deref().map(parse_target_kind),
        target_key: row.get(5)?,
        target_payload_json: row.get(6)?,
        skill_name: row.get(7)?,
        request_system_prompt: row.get(8)?,
        request_user_prompt: row.get(9)?,
        raw_response: row.get(10)?,
        http_status: row.get(11)?,
        input_tokens: row.get(12)?,
        output_tokens: row.get(13)?,
        total_tokens: row.get(14)?,
        duration_ms: row.get(15)?,
        error_code: row.get(16)?,
        error_message: row.get(17)?,
        created_at: row.get(18)?,
    })
}

/// Stable page cursor: `created_at:id`, ordered DESC. Frontend never parses it.
fn encode_cursor(created_at: i64, id: &str) -> String {
    format!("{created_at}:{id}")
}

fn parse_cursor(cursor: Option<&str>) -> Result<(Option<i64>, Option<String>)> {
    let Some(cursor) = cursor else {
        return Ok((None, None));
    };
    let Some((created_at, id)) = cursor.split_once(':') else {
        bail!("invalid AI list cursor");
    };
    let created_at = created_at
        .parse::<i64>()
        .context("invalid AI list cursor")?;
    if id.is_empty() {
        bail!("invalid AI list cursor");
    }
    Ok((Some(created_at), Some(id.to_string())))
}

fn load_job(connection: &Connection, id: &str) -> Result<Option<AiJobRecord>> {
    Ok(connection
        .query_row(
            "SELECT id,batch_id,ordinal,target_kind,target_key,target_payload_json,skill_name,
                    expected_source_hash,status,priority,attempt_count,manual_retry_count,
                    correction_attempted,cancel_requested,next_retry_at,error_code,error_message,
                    created_at,updated_at,started_at,finished_at
             FROM ai_analysis_jobs WHERE id=?1",
            params![id],
            map_job_row,
        )
        .optional()?)
}

fn map_analysis_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiAnalysisRecord> {
    let kind: String = row.get(1)?;
    Ok(AiAnalysisRecord {
        id: row.get(0)?,
        target_kind: parse_target_kind(&kind),
        target_key: row.get(2)?,
        target_payload_json: row.get(3)?,
        skill_name: row.get(4)?,
        source_hash: row.get(5)?,
        schema_version: row.get(6)?,
        prompt_version: row.get(7)?,
        output_language: row.get(8)?,
        one_line: row.get(9)?,
        result_json: row.get(10)?,
        provider: row.get(11)?,
        model: row.get(12)?,
        input_tokens: row.get(13)?,
        output_tokens: row.get(14)?,
        total_tokens: row.get(15)?,
        analyzed_at: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn map_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiJobRecord> {
    let kind: String = row.get(3)?;
    let status: String = row.get(8)?;
    Ok(AiJobRecord {
        id: row.get(0)?,
        batch_id: row.get(1)?,
        ordinal: row.get(2)?,
        target_kind: parse_target_kind(&kind),
        target_key: row.get(4)?,
        target_payload_json: row.get(5)?,
        skill_name: row.get(6)?,
        expected_source_hash: row.get(7)?,
        status: parse_job_status(&status),
        priority: row.get(9)?,
        attempt_count: row.get(10)?,
        manual_retry_count: row.get(11)?,
        correction_attempted: row.get::<_, i64>(12)? != 0,
        cancel_requested: row.get::<_, i64>(13)? != 0,
        next_retry_at: row.get(14)?,
        error_code: row.get(15)?,
        error_message: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        started_at: row.get(19)?,
        finished_at: row.get(20)?,
    })
}

fn parse_target_kind(value: &str) -> AiTargetKind {
    match value {
        "managed" => AiTargetKind::Managed,
        "global_local" => AiTargetKind::GlobalLocal,
        _ => AiTargetKind::ProjectLocal,
    }
}

fn parse_batch_status(value: &str) -> AiBatchStatus {
    match value {
        "queued" => AiBatchStatus::Queued,
        "running" => AiBatchStatus::Running,
        "paused" => AiBatchStatus::Paused,
        "cancelling" => AiBatchStatus::Cancelling,
        "cancelled" => AiBatchStatus::Cancelled,
        _ => AiBatchStatus::Completed,
    }
}

fn parse_job_status(value: &str) -> AiJobStatus {
    match value {
        "queued" => AiJobStatus::Queued,
        "running" => AiJobStatus::Running,
        "retry_wait" => AiJobStatus::RetryWait,
        "interrupted" => AiJobStatus::Interrupted,
        "succeeded" => AiJobStatus::Succeeded,
        "failed" => AiJobStatus::Failed,
        _ => AiJobStatus::Cancelled,
    }
}

/// Finalize a batch when every job is terminal: uncancelled batches become
/// `completed` and clear both control flags in the same transaction; cancelled
/// batches become `cancelled`.
fn finalize_batch(transaction: &Transaction<'_>, batch_id: &str, now: i64) -> Result<()> {
    let remaining: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM ai_analysis_jobs
         WHERE batch_id=?1 AND status NOT IN ('succeeded','failed','cancelled')",
        params![batch_id],
        |row| row.get(0),
    )?;
    if remaining != 0 {
        return Ok(());
    }
    let cancel_requested: i64 = transaction.query_row(
        "SELECT cancel_requested FROM ai_analysis_batches WHERE id=?1",
        params![batch_id],
        |row| row.get(0),
    )?;
    if cancel_requested != 0 {
        transaction.execute(
            "UPDATE ai_analysis_batches SET status='cancelled', pause_requested=0,
                    finished_at=?1, updated_at=?1
             WHERE id=?2 AND status IN ('queued','running','paused','cancelling')",
            params![now, batch_id],
        )?;
    } else {
        transaction.execute(
            "UPDATE ai_analysis_batches SET status='completed', pause_requested=0,
                    cancel_requested=0, finished_at=?1, updated_at=?1
             WHERE id=?2 AND status IN ('queued','running','paused')",
            params![now, batch_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ai::logs::save_log;
    use crate::core::ai::types::{
        AiBatchStatus, AiJobStatus, AiLogEventKind, AiLogRecord, AiTargetKind,
    };
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, SkillStore) {
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("ai-test.db")).unwrap();
        (directory, store)
    }

    fn analysis(id: &str) -> AiAnalysisRecord {
        AiAnalysisRecord {
            id: id.into(),
            target_kind: AiTargetKind::Managed,
            target_key: "[\"skill-1\"]".into(),
            target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"skill-1\"}".into(),
            skill_name: "Skill 1".into(),
            source_hash: "hash".into(),
            schema_version: 1,
            prompt_version: "v1".into(),
            output_language: "en".into(),
            one_line: "Summary".into(),
            result_json: "{}".into(),
            provider: "custom".into(),
            model: "model".into(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            analyzed_at: 1,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn batch(id: &str, valid_documents: i64) -> AiBatchRecord {
        AiBatchRecord {
            id: id.into(),
            status: AiBatchStatus::Queued,
            provider: "custom".into(),
            base_url: "https://example.invalid/v1/".into(),
            model: "model".into(),
            output_language: "en".into(),
            prompt_version: "v1".into(),
            schema_version: 1,
            timeout_seconds: 60,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
            estimated_input_tokens: 10,
            estimated_output_tokens: 5,
            estimated_cost_micros: None,
            estimated_max_retry_cost_micros: None,
            total_targets: valid_documents,
            valid_documents,
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

    fn job(id: &str, batch_id: &str, ordinal: i64) -> AiJobRecord {
        AiJobRecord {
            id: id.into(),
            batch_id: batch_id.into(),
            ordinal,
            target_kind: AiTargetKind::Managed,
            target_key: "[\"skill-1\"]".into(),
            target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"skill-1\"}".into(),
            skill_name: "Skill 1".into(),
            expected_source_hash: "hash".into(),
            status: AiJobStatus::Queued,
            priority: 0,
            attempt_count: 0,
            manual_retry_count: 0,
            correction_attempted: false,
            cancel_requested: false,
            next_retry_at: None,
            error_code: None,
            error_message: None,
            created_at: 1,
            updated_at: 1,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn analysis_target_is_unique() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);

        repository.insert_analysis(&analysis("analysis-1")).unwrap();
        assert!(repository.insert_analysis(&analysis("analysis-2")).is_err());
    }

    #[test]
    fn active_job_target_is_unique_and_batch_write_rolls_back() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        let jobs = [job("job-1", "batch-1", 0), job("job-2", "batch-1", 1)];

        // The second queued job violates the partial active-target index; the
        // batch and first job must not survive the transaction failure.
        assert!(repository
            .insert_batch_with_jobs(&batch("batch-1", 2), &jobs)
            .is_err());

        store
            .with_ai_connection(|connection| {
                let batches: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM ai_analysis_batches WHERE id = 'batch-1'",
                    [],
                    |row| row.get(0),
                )?;
                let jobs: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM ai_analysis_jobs WHERE batch_id = 'batch-1'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!((batches, jobs), (0, 0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn batch_target_counts_must_balance() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        let mut invalid = batch("batch-invalid", 1);
        invalid.total_targets = 2;

        assert!(repository.insert_batch(&invalid).is_err());
    }

    #[test]
    fn log_event_kind_check_rejects_unknown_values() {
        let (_directory, store) = store();

        let result = store.with_ai_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO ai_analysis_logs (id,event_kind,created_at) VALUES ('bad-log','credentials_dumped',1)",
                [],
            )?;
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn repository_inserts_safe_log_record() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        let log = AiLogRecord {
            id: "log-1".into(),
            event_kind: AiLogEventKind::Recovery,
            job_id: None,
            batch_id: None,
            target_kind: None,
            target_key: None,
            target_payload_json: None,
            skill_name: None,
            request_system_prompt: None,
            request_user_prompt: None,
            raw_response: None,
            http_status: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            duration_ms: None,
            error_code: Some("interrupted".into()),
            error_message: Some("application restarted".into()),
            created_at: 1,
        };

        save_log(&store, log, None).unwrap();
        let count: i64 = store
            .with_ai_connection(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM ai_analysis_logs", [], |row| {
                        row.get(0)
                    })
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn standalone_job_requires_existing_batch() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);

        assert!(repository
            .insert_job(&job("orphan", "missing-batch", 0))
            .is_err());
    }

    fn claim_batch(id: &str, valid_documents: i64) -> AiBatchRecord {
        AiBatchRecord {
            id: id.into(),
            status: AiBatchStatus::Queued,
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1/".into(),
            model: "local-model".into(),
            output_language: "en".into(),
            prompt_version: "v1".into(),
            schema_version: 1,
            timeout_seconds: 60,
            input_price_micros_per_million: None,
            output_price_micros_per_million: None,
            estimated_input_tokens: 10,
            estimated_output_tokens: 5,
            estimated_cost_micros: None,
            estimated_max_retry_cost_micros: None,
            total_targets: valid_documents,
            valid_documents,
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

    fn claim_job(id: &str, batch_id: &str, ordinal: i64, target: &str) -> AiJobRecord {
        AiJobRecord {
            id: id.into(),
            batch_id: batch_id.into(),
            ordinal,
            target_kind: AiTargetKind::Managed,
            target_key: format!("[\"{target}\"]"),
            target_payload_json: format!("{{\"kind\":\"managed\",\"skill_id\":\"{target}\"}}"),
            skill_name: target.into(),
            expected_source_hash: "hash".into(),
            status: AiJobStatus::Queued,
            priority: 0,
            attempt_count: 0,
            manual_retry_count: 0,
            correction_attempted: false,
            cancel_requested: false,
            next_retry_at: None,
            error_code: None,
            error_message: None,
            created_at: 1,
            updated_at: 1,
            started_at: None,
            finished_at: None,
        }
    }

    fn running_log(job_id: &str, now: i64) -> crate::core::ai::logs::SanitizedAiLogRecord {
        crate::core::ai::logs::sanitized_record(
            AiLogRecord {
                id: uuid::Uuid::new_v4().to_string(),
                event_kind: AiLogEventKind::RequestStarted,
                job_id: Some(job_id.into()),
                batch_id: None,
                target_kind: None,
                target_key: None,
                target_payload_json: None,
                skill_name: None,
                request_system_prompt: None,
                request_user_prompt: None,
                raw_response: None,
                http_status: None,
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                duration_ms: None,
                error_code: None,
                error_message: None,
                created_at: now,
            },
            None,
        )
    }

    fn job_status(store: &SkillStore, job_id: &str) -> String {
        store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT status FROM ai_analysis_jobs WHERE id=?1",
                        params![job_id],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    fn batch_status(store: &SkillStore, batch_id: &str) -> String {
        store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT status FROM ai_analysis_batches WHERE id=?1",
                        params![batch_id],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    #[test]
    fn claim_picks_jobs_in_order_and_turns_batch_running() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-claim", 2),
                &[
                    claim_job("job-a", "batch-claim", 0, "skill-a"),
                    claim_job("job-b", "batch-claim", 1, "skill-b"),
                ],
            )
            .unwrap();

        let claimed = repository.claim_next_job(100).unwrap().unwrap();
        assert_eq!(claimed.job.id, "job-a");
        assert_eq!(claimed.job.status, AiJobStatus::Running);
        assert_eq!(claimed.batch.status, AiBatchStatus::Running);
        assert_eq!(batch_status(&store, "batch-claim"), "running");
        assert_eq!(job_status(&store, "job-a"), "running");
        assert_eq!(job_status(&store, "job-b"), "queued");
    }

    #[test]
    fn claim_skips_paused_or_cancelled_batches() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        let mut paused = claim_batch("batch-paused", 1);
        paused.pause_requested = true;
        paused.status = AiBatchStatus::Paused;
        repository.insert_batch(&paused).unwrap();
        repository
            .insert_job(&claim_job("job-paused", "batch-paused", 0, "skill-paused"))
            .unwrap();

        let mut cancelled = claim_batch("batch-cancelled", 1);
        cancelled.cancel_requested = true;
        cancelled.status = AiBatchStatus::Cancelling;
        repository.insert_batch(&cancelled).unwrap();
        repository
            .insert_job(&claim_job(
                "job-cancelled",
                "batch-cancelled",
                0,
                "skill-cancelled",
            ))
            .unwrap();

        assert!(repository.claim_next_job(100).unwrap().is_none());
    }

    #[test]
    fn reserve_enforces_budget_and_cancel_linearization() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-budget", 1),
                &[claim_job("job-budget", "batch-budget", 0, "skill-budget")],
            )
            .unwrap();
        repository.claim_next_job(100).unwrap();

        for attempt in 1..=3 {
            let reservation = repository
                .reserve_http_attempt(
                    "job-budget",
                    false,
                    100 + attempt,
                    running_log("job-budget", 100 + attempt),
                )
                .unwrap();
            assert_eq!(
                reservation,
                AttemptReservation::Reserved {
                    attempt_number: attempt
                }
            );
        }
        assert_eq!(
            repository
                .reserve_http_attempt("job-budget", false, 200, running_log("job-budget", 200))
                .unwrap(),
            AttemptReservation::NoBudget
        );

        let log = running_log("job-budget", 300);
        let outcome = repository
            .complete_success(
                "job-budget",
                &AiAnalysisRecord {
                    id: "analysis-budget".into(),
                    target_kind: AiTargetKind::Managed,
                    target_key: "[\"skill-budget\"]".into(),
                    target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"skill-budget\"}"
                        .into(),
                    skill_name: "budget".into(),
                    source_hash: "hash".into(),
                    schema_version: 1,
                    prompt_version: "v1".into(),
                    output_language: "en".into(),
                    one_line: "Summary".into(),
                    result_json: "{}".into(),
                    provider: "ollama".into(),
                    model: "local-model".into(),
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    total_tokens: Some(2),
                    analyzed_at: 300,
                    created_at: 300,
                    updated_at: 300,
                },
                log,
                300,
            )
            .unwrap();
        assert_eq!(outcome, CompleteOutcome::Succeeded);
        assert_eq!(job_status(&store, "job-budget"), "succeeded");
        assert_eq!(batch_status(&store, "batch-budget"), "completed");
    }

    #[test]
    fn cancel_before_success_commit_discards_result() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-cancel", 1),
                &[claim_job("job-cancel", "batch-cancel", 0, "skill-cancel")],
            )
            .unwrap();
        repository.claim_next_job(100).unwrap();

        // Simulate the phase-4 cancel command flagging the job and batch.
        store
            .with_ai_transaction(|transaction| {
                transaction.execute(
                    "UPDATE ai_analysis_jobs SET cancel_requested=1 WHERE id='job-cancel'",
                    [],
                )?;
                transaction.execute(
                    "UPDATE ai_analysis_batches SET cancel_requested=1, status='cancelling' WHERE id='batch-cancel'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let outcome = repository
            .complete_success(
                "job-cancel",
                &AiAnalysisRecord {
                    id: "analysis-cancel".into(),
                    target_kind: AiTargetKind::Managed,
                    target_key: "[\"skill-cancel\"]".into(),
                    target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"skill-cancel\"}"
                        .into(),
                    skill_name: "cancel".into(),
                    source_hash: "hash".into(),
                    schema_version: 1,
                    prompt_version: "v1".into(),
                    output_language: "en".into(),
                    one_line: "Must not persist".into(),
                    result_json: "{}".into(),
                    provider: "ollama".into(),
                    model: "local-model".into(),
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    analyzed_at: 200,
                    created_at: 200,
                    updated_at: 200,
                },
                running_log("job-cancel", 200),
                200,
            )
            .unwrap();
        assert_eq!(outcome, CompleteOutcome::Cancelled);
        assert_eq!(job_status(&store, "job-cancel"), "cancelled");
        let analysis_count: i64 = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM skill_ai_analyses WHERE id='analysis-cancel'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(analysis_count, 0);
        assert_eq!(batch_status(&store, "batch-cancel"), "cancelled");
    }

    #[test]
    fn fail_job_retries_then_terminates_on_budget_exhaustion() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-retry", 1),
                &[claim_job("job-retry", "batch-retry", 0, "skill-retry")],
            )
            .unwrap();
        repository.claim_next_job(100).unwrap();

        repository
            .reserve_http_attempt("job-retry", false, 101, running_log("job-retry", 101))
            .unwrap();
        let outcome = repository
            .fail_job(
                "job-retry",
                "rate_limited",
                "rate limited",
                Some(2),
                running_log("job-retry", 101),
                101,
            )
            .unwrap();
        assert!(matches!(outcome, FailOutcome::RetryScheduled(_)));
        assert_eq!(job_status(&store, "job-retry"), "retry_wait");

        // After the wait elapses the job becomes claimable again.
        let claimed = repository.claim_next_job(5_000).unwrap().unwrap();
        assert_eq!(claimed.job.id, "job-retry");

        // Exhaust the remaining two attempts, then the third failure is final.
        repository
            .reserve_http_attempt("job-retry", false, 5_001, running_log("job-retry", 5_001))
            .unwrap();
        repository
            .reserve_http_attempt("job-retry", false, 5_002, running_log("job-retry", 5_002))
            .unwrap();
        assert_eq!(
            repository
                .reserve_http_attempt("job-retry", false, 5_003, running_log("job-retry", 5_003))
                .unwrap(),
            AttemptReservation::NoBudget
        );
        let outcome = repository
            .fail_job(
                "job-retry",
                "provider_response",
                "temporarily unavailable",
                None,
                running_log("job-retry", 5_003),
                5_003,
            )
            .unwrap();
        assert_eq!(outcome, FailOutcome::Failed);
        assert_eq!(job_status(&store, "job-retry"), "failed");
        assert_eq!(batch_status(&store, "batch-retry"), "completed");
    }

    #[test]
    fn recovery_requeues_interrupted_without_resetting_attempts() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-recover", 2),
                &[
                    claim_job("job-recover-a", "batch-recover", 0, "skill-recover-a"),
                    claim_job("job-recover-b", "batch-recover", 1, "skill-recover-b"),
                ],
            )
            .unwrap();
        repository.claim_next_job(100).unwrap();
        store
            .with_ai_transaction(|transaction| {
                transaction.execute(
                    "UPDATE ai_analysis_jobs SET attempt_count=2 WHERE id='job-recover-a'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let summary = repository.recover_on_startup(200).unwrap();
        assert_eq!(summary.interrupted, 1);
        assert_eq!(summary.requeued, 1);
        assert_eq!(job_status(&store, "job-recover-a"), "queued");
        let attempts: i64 = store
            .with_ai_connection(|connection| {
                connection
                    .query_row(
                        "SELECT attempt_count FROM ai_analysis_jobs WHERE id='job-recover-a'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(batch_status(&store, "batch-recover"), "queued");
    }

    #[test]
    fn recovery_cancels_flagged_batches_and_preserves_paused_ones() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        let mut cancel_batch = claim_batch("batch-cancel-flag", 1);
        cancel_batch.cancel_requested = true;
        cancel_batch.status = AiBatchStatus::Cancelling;
        repository.insert_batch(&cancel_batch).unwrap();
        let mut cancel_job = claim_job("job-cancel-flag", "batch-cancel-flag", 0, "skill-cf");
        cancel_job.status = AiJobStatus::Interrupted;
        cancel_job.cancel_requested = true;
        repository.insert_job(&cancel_job).unwrap();

        let mut paused_batch = claim_batch("batch-pause-flag", 1);
        paused_batch.pause_requested = true;
        paused_batch.status = AiBatchStatus::Paused;
        repository.insert_batch(&paused_batch).unwrap();
        let mut paused_job = claim_job("job-pause-flag", "batch-pause-flag", 0, "skill-pf");
        paused_job.status = AiJobStatus::Queued;
        repository.insert_job(&paused_job).unwrap();

        repository.recover_on_startup(300).unwrap();
        assert_eq!(job_status(&store, "job-cancel-flag"), "cancelled");
        assert_eq!(batch_status(&store, "batch-cancel-flag"), "cancelled");
        assert_eq!(job_status(&store, "job-pause-flag"), "queued");
        assert_eq!(batch_status(&store, "batch-pause-flag"), "paused");
    }

    #[test]
    fn pause_blocks_claims_and_resume_requeues() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-pause-resume", 1),
                &[claim_job("job-pr", "batch-pause-resume", 0, "skill-pr")],
            )
            .unwrap();

        let paused = repository
            .pause_batch("batch-pause-resume", 100)
            .unwrap()
            .unwrap();
        assert_eq!(paused.status, AiBatchStatus::Paused);
        assert!(paused.pause_requested);
        assert!(repository.claim_next_job(100).unwrap().is_none());

        let resumed = repository
            .resume_batch("batch-pause-resume", 101)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, AiBatchStatus::Queued);
        assert!(!resumed.pause_requested);
        assert!(repository.claim_next_job(101).unwrap().is_some());
    }

    #[test]
    fn cancel_batch_cancels_queued_jobs_and_reports_running_ids() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-cancel-all", 2),
                &[
                    claim_job("job-ca", "batch-cancel-all", 0, "skill-ca"),
                    claim_job("job-cb", "batch-cancel-all", 1, "skill-cb"),
                ],
            )
            .unwrap();
        repository.claim_next_job(100).unwrap();

        let (batch, running) = repository.cancel_batch("batch-cancel-all", 200).unwrap();
        let batch = batch.unwrap();
        assert_eq!(batch.status, AiBatchStatus::Cancelling);
        assert_eq!(running, vec!["job-ca".to_string()]);
        assert_eq!(job_status(&store, "job-ca"), "running");
        assert_eq!(job_status(&store, "job-cb"), "cancelled");

        // Simulate the running job finishing after cancellation: result is
        // discarded and the batch reaches cancelled.
        let outcome = repository
            .complete_success(
                "job-ca",
                &AiAnalysisRecord {
                    id: "analysis-ca".into(),
                    target_kind: AiTargetKind::Managed,
                    target_key: "[\"skill-ca\"]".into(),
                    target_payload_json: "{\"kind\":\"managed\",\"skill_id\":\"skill-ca\"}".into(),
                    skill_name: "ca".into(),
                    source_hash: "hash".into(),
                    schema_version: 1,
                    prompt_version: "v1".into(),
                    output_language: "en".into(),
                    one_line: "discarded".into(),
                    result_json: "{}".into(),
                    provider: "ollama".into(),
                    model: "local".into(),
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    analyzed_at: 201,
                    created_at: 201,
                    updated_at: 201,
                },
                running_log("job-ca", 201),
                201,
            )
            .unwrap();
        assert_eq!(outcome, CompleteOutcome::Cancelled);
        assert_eq!(batch_status(&store, "batch-cancel-all"), "cancelled");
    }

    #[test]
    fn list_batches_uses_stable_descending_cursor() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        for index in 0..3 {
            let batch_id = format!("batch-list-{index}");
            repository.insert_batch(&claim_batch(&batch_id, 0)).unwrap();
        }

        let (first, cursor) = repository.list_batches(None, None, 2).unwrap();
        assert_eq!(first.len(), 2);
        assert!(cursor.is_some());
        let (second, next) = repository.list_batches(None, cursor.as_deref(), 2).unwrap();
        assert_eq!(second.len(), 1);
        assert!(next.is_none());
        let ids: Vec<String> = first
            .iter()
            .chain(second.iter())
            .map(|batch| batch.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                "batch-list-2".to_string(),
                "batch-list-1".to_string(),
                "batch-list-0".to_string()
            ]
        );
    }

    #[test]
    fn cancel_job_queued_moves_terminal_and_finalizes_batch() {
        let (_directory, store) = store();
        let repository = AiRepository::new(&store);
        repository
            .insert_batch_with_jobs(
                &claim_batch("batch-single-cancel", 1),
                &[claim_job(
                    "job-single-cancel",
                    "batch-single-cancel",
                    0,
                    "skill-sc",
                )],
            )
            .unwrap();

        assert_eq!(
            repository.cancel_job("job-single-cancel", 100).unwrap(),
            CancelJobOutcome::Cancelled
        );
        assert_eq!(job_status(&store, "job-single-cancel"), "cancelled");
        assert_eq!(batch_status(&store, "batch-single-cancel"), "completed");

        // Terminal states reject cancellation; cancelled is idempotent.
        assert_eq!(
            repository.cancel_job("job-single-cancel", 101).unwrap(),
            CancelJobOutcome::Cancelled
        );
    }
}
