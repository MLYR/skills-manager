use anyhow::{bail, Context, Result};
use rusqlite::{params, Transaction};

use super::logs::SanitizedAiLogRecord;
use super::types::{AiAnalysisRecord, AiBatchRecord, AiJobRecord};
use crate::core::skill_store::SkillStore;

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
        // The wrapper's private constructor proves every persisted text field
        // crossed the logs module's exact-key and generic redaction gate.
        let log = sanitized.as_record();
        self.store.with_ai_transaction(|transaction| {
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
}
