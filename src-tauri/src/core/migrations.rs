use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// Current schema version. Bump this when adding a new migration.
const LATEST_VERSION: u32 = 8;

/// Run all pending migrations on the database.
///
/// - New databases: creates full schema and sets version to LATEST_VERSION.
/// - Existing databases (user_version == 0): runs incremental migrations
///   to bring them up to date.
/// - Databases newer than this app version: returns an error.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current > LATEST_VERSION {
        bail!(
            "Database schema version ({current}) is newer than this app supports ({LATEST_VERSION}). \
             Please upgrade the application."
        );
    }

    if current == LATEST_VERSION {
        return Ok(());
    }

    // Run each migration step in a transaction
    for version in current..LATEST_VERSION {
        run_migration_transaction(
            conn,
            version,
            |connection| migrate_step(connection, version),
            |connection| {
                connection
                    .pragma_update(None, "user_version", version + 1)
                    .map_err(anyhow::Error::from)
            },
            |connection| {
                connection
                    .execute_batch("COMMIT")
                    .map_err(anyhow::Error::from)
            },
        )?;
    }

    Ok(())
}

fn run_migration_transaction<M, U, C>(
    conn: &Connection,
    version: u32,
    migrate: M,
    update_version: U,
    commit: C,
) -> Result<()>
where
    M: FnOnce(&Connection) -> Result<()>,
    U: FnOnce(&Connection) -> Result<()>,
    C: FnOnce(&Connection) -> Result<()>,
{
    let next_version = version + 1;
    conn.execute_batch("BEGIN EXCLUSIVE").with_context(|| {
        format!("failed to begin migration from version {version} to {next_version}")
    })?;

    let result: Result<()> = (|| {
        migrate(conn).context("migration step failed")?;
        update_version(conn).context("failed to update database user_version")?;
        commit(conn).context("failed to commit database migration")?;
        Ok(())
    })();

    if let Err(error) = result {
        // Every post-BEGIN failure attempts rollback, including user_version
        // and COMMIT failures, so callers can safely reuse this connection.
        let rollback_context = match conn.execute_batch("ROLLBACK") {
            Ok(()) => "transaction rolled back".to_string(),
            Err(rollback_error) => format!("rollback attempt failed: {rollback_error}"),
        };
        return Err(error).with_context(|| {
            format!("migration from version {version} to {next_version} failed; {rollback_context}")
        });
    }

    Ok(())
}

/// Execute a single migration step: version N → N+1.
fn migrate_step(conn: &Connection, from_version: u32) -> Result<()> {
    match from_version {
        0 => migrate_v0_to_v1(conn),
        1 => migrate_v1_to_v2(conn),
        2 => migrate_v2_to_v3(conn),
        3 => migrate_v3_to_v4(conn),
        4 => migrate_v4_to_v5(conn),
        5 => migrate_v5_to_v6(conn),
        6 => migrate_v6_to_v7(conn),
        7 => migrate_v7_to_v8(conn),
        _ => bail!("unknown migration version: {from_version}"),
    }
}

/// v0 → v1: Initial schema.
///
/// For new databases this creates all tables from scratch.
/// For existing pre-migration databases, the `CREATE TABLE IF NOT EXISTS`
/// statements are no-ops, and the `add_column_if_missing` calls handle
/// columns that were added incrementally before the migration system existed.
fn migrate_v0_to_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS skills (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            source_type TEXT NOT NULL,
            source_ref TEXT,
            source_ref_resolved TEXT,
            source_subpath TEXT,
            source_branch TEXT,
            source_revision TEXT,
            remote_revision TEXT,
            central_path TEXT NOT NULL UNIQUE,
            content_hash TEXT,
            enabled INTEGER DEFAULT 1,
            created_at INTEGER,
            updated_at INTEGER,
            status TEXT DEFAULT 'ok',
            update_status TEXT DEFAULT 'unknown',
            last_checked_at INTEGER,
            last_check_error TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_skills_name ON skills(name);

        CREATE TABLE IF NOT EXISTS skill_targets (
            id TEXT PRIMARY KEY,
            skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
            tool TEXT NOT NULL,
            target_path TEXT NOT NULL,
            mode TEXT NOT NULL,
            status TEXT DEFAULT 'ok',
            synced_at INTEGER,
            last_error TEXT,
            source_hash TEXT,
            UNIQUE(skill_id, tool)
        );

        CREATE TABLE IF NOT EXISTS discovered_skills (
            id TEXT PRIMARY KEY,
            tool TEXT NOT NULL,
            found_path TEXT NOT NULL,
            name_guess TEXT,
            fingerprint TEXT,
            found_at INTEGER NOT NULL,
            imported_skill_id TEXT REFERENCES skills(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS skillssh_cache (
            cache_key TEXT PRIMARY KEY,
            data TEXT NOT NULL,
            fetched_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS scenarios (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            description TEXT,
            icon TEXT,
            sort_order INTEGER DEFAULT 0,
            created_at INTEGER,
            updated_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS scenario_skills (
            scenario_id TEXT NOT NULL REFERENCES scenarios(id) ON DELETE CASCADE,
            skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
            added_at INTEGER,
            PRIMARY KEY(scenario_id, skill_id)
        );

        CREATE TABLE IF NOT EXISTS scenario_skill_tools (
            scenario_id TEXT NOT NULL REFERENCES scenarios(id) ON DELETE CASCADE,
            skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
            tool TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY(scenario_id, skill_id, tool)
        );

        CREATE TABLE IF NOT EXISTS active_scenario (
            key TEXT PRIMARY KEY DEFAULT 'current',
            scenario_id TEXT REFERENCES scenarios(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            workspace_type TEXT NOT NULL DEFAULT 'project',
            linked_agent_key TEXT,
            linked_agent_name TEXT,
            disabled_path TEXT,
            sort_order INTEGER DEFAULT 0,
            created_at INTEGER,
            updated_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS skill_tags (
            skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
            tag TEXT NOT NULL,
            PRIMARY KEY(skill_id, tag)
        );
        CREATE INDEX IF NOT EXISTS idx_skill_tags_tag ON skill_tags(tag);
        ",
    )?;

    // For pre-migration databases: add columns that didn't exist in the original schema.
    // For new databases these are already in the CREATE TABLE, so the calls are no-ops.
    add_column_if_missing(conn, "scenarios", "icon", "TEXT")?;
    add_column_if_missing(conn, "skills", "source_ref_resolved", "TEXT")?;
    add_column_if_missing(conn, "skills", "source_subpath", "TEXT")?;
    add_column_if_missing(conn, "skills", "source_branch", "TEXT")?;
    add_column_if_missing(conn, "skills", "remote_revision", "TEXT")?;
    add_column_if_missing(conn, "skills", "update_status", "TEXT DEFAULT 'unknown'")?;
    add_column_if_missing(conn, "skills", "last_checked_at", "INTEGER")?;
    add_column_if_missing(conn, "skills", "last_check_error", "TEXT")?;

    Ok(())
}

/// v1 → v2: Add per-scenario, per-skill tool toggle table.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS scenario_skill_tools (
            scenario_id TEXT NOT NULL REFERENCES scenarios(id) ON DELETE CASCADE,
            skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
            tool TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY(scenario_id, skill_id, tool)
        );
        ",
    )?;
    Ok(())
}

/// v2 → v3: Add sort_order to scenario_skills for drag-and-drop reordering.
fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "scenario_skills", "sort_order", "INTEGER DEFAULT 0")?;
    Ok(())
}

/// v3 → v4: Expand projects into generic workspace records.
fn migrate_v3_to_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            workspace_type TEXT NOT NULL DEFAULT 'project',
            linked_agent_key TEXT,
            linked_agent_name TEXT,
            disabled_path TEXT,
            sort_order INTEGER DEFAULT 0,
            created_at INTEGER,
            updated_at INTEGER
        );
        ",
    )?;
    add_column_if_missing(
        conn,
        "projects",
        "workspace_type",
        "TEXT NOT NULL DEFAULT 'project'",
    )?;
    add_column_if_missing(conn, "projects", "linked_agent_key", "TEXT")?;
    add_column_if_missing(conn, "projects", "linked_agent_name", "TEXT")?;
    add_column_if_missing(conn, "projects", "disabled_path", "TEXT")?;
    Ok(())
}

/// v4 → v5: Add audit log table — append-only history of user/system actions.
fn migrate_v4_to_v5(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            action TEXT NOT NULL,
            skill_id TEXT,
            skill_name TEXT,
            tool TEXT,
            success INTEGER NOT NULL,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_audit_log_ts ON audit_log(ts);
        ",
    )?;
    Ok(())
}

/// v5 → v6: Add `source_hash` to `skill_targets`. Lets the sync engine
/// skip a Copy-mode resync when the central skill content has not
/// changed since the last successful sync, avoiding the per-startup
/// recursive copy that pinned Windows users on issue #153.
///
/// Existing rows get NULL, which is treated as "no recorded hash" and
/// forces one copy on the first post-upgrade sync. No backfill needed.
fn migrate_v5_to_v6(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "skill_targets", "source_hash", "TEXT")?;
    Ok(())
}

/// v6 → v7: pending-conflict projection for the object merge engine
/// (merge-engine design §4). A local UI cache only — the source of truth is
/// the commit trailers plus `refs/skills-manager/conflict/*`, from which
/// this table is rebuilt at startup and after every merge.
fn migrate_v6_to_v7(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS pending_conflicts (
            skill_id TEXT PRIMARY KEY,
            theirs_commit TEXT NOT NULL,
            theirs_path TEXT,
            detected_at INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

/// v7 → v8: persist AI analysis results, confirmed batches, jobs, and safe logs.
///
/// These tables intentionally have no credential or request-header columns so
/// later service code cannot accidentally cross the system-keyring boundary.
fn migrate_v7_to_v8(conn: &Connection) -> Result<()> {
    // Keep the complete schema in one migration step so the outer per-version
    // transaction can roll back every table and index on any intermediate error.
    conn.execute_batch(
        "
        CREATE TABLE skill_ai_analyses (
            id TEXT PRIMARY KEY,
            target_kind TEXT NOT NULL CHECK(target_kind IN ('managed','global_local','project_local')),
            target_key TEXT NOT NULL,
            target_payload_json TEXT NOT NULL,
            skill_name TEXT NOT NULL,
            source_hash TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            prompt_version TEXT NOT NULL,
            output_language TEXT NOT NULL,
            one_line TEXT NOT NULL,
            result_json TEXT NOT NULL,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
            output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
            total_tokens INTEGER CHECK(total_tokens IS NULL OR total_tokens >= 0),
            analyzed_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE ai_analysis_batches (
            id TEXT PRIMARY KEY,
            status TEXT NOT NULL CHECK(status IN ('queued','running','paused','cancelling','completed','cancelled')),
            provider TEXT NOT NULL,
            base_url TEXT NOT NULL,
            model TEXT NOT NULL,
            output_language TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            schema_version INTEGER NOT NULL CHECK(schema_version = 1),
            timeout_seconds INTEGER NOT NULL CHECK(timeout_seconds BETWEEN 1 AND 300),
            input_price_micros_per_million INTEGER CHECK(input_price_micros_per_million IS NULL OR input_price_micros_per_million >= 0),
            output_price_micros_per_million INTEGER CHECK(output_price_micros_per_million IS NULL OR output_price_micros_per_million >= 0),
            estimated_input_tokens INTEGER NOT NULL CHECK(estimated_input_tokens >= 0),
            estimated_output_tokens INTEGER NOT NULL CHECK(estimated_output_tokens >= 0),
            estimated_cost_micros INTEGER CHECK(estimated_cost_micros IS NULL OR estimated_cost_micros >= 0),
            estimated_max_retry_cost_micros INTEGER CHECK(estimated_max_retry_cost_micros IS NULL OR estimated_max_retry_cost_micros >= 0),
            total_targets INTEGER NOT NULL CHECK(total_targets >= 0),
            valid_documents INTEGER NOT NULL CHECK(valid_documents >= 0),
            missing_documents INTEGER NOT NULL CHECK(missing_documents >= 0),
            unreadable_documents INTEGER NOT NULL CHECK(unreadable_documents >= 0),
            skipped_targets INTEGER NOT NULL CHECK(skipped_targets >= 0),
            pause_requested INTEGER NOT NULL DEFAULT 0 CHECK(pause_requested IN (0, 1)),
            cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0, 1)),
            confirmed_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            finished_at INTEGER,
            CHECK(total_targets = valid_documents + missing_documents + unreadable_documents + skipped_targets)
        );

        CREATE TABLE ai_analysis_jobs (
            id TEXT PRIMARY KEY,
            batch_id TEXT NOT NULL REFERENCES ai_analysis_batches(id),
            ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
            target_kind TEXT NOT NULL CHECK(target_kind IN ('managed','global_local','project_local')),
            target_key TEXT NOT NULL,
            target_payload_json TEXT NOT NULL,
            skill_name TEXT NOT NULL,
            expected_source_hash TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('queued','running','retry_wait','interrupted','succeeded','failed','cancelled')),
            priority INTEGER NOT NULL DEFAULT 0 CHECK(priority >= 0),
            attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
            manual_retry_count INTEGER NOT NULL DEFAULT 0 CHECK(manual_retry_count >= 0),
            correction_attempted INTEGER NOT NULL DEFAULT 0 CHECK(correction_attempted IN (0, 1)),
            cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0, 1)),
            next_retry_at INTEGER,
            error_code TEXT,
            error_message TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            started_at INTEGER,
            finished_at INTEGER
        );

        CREATE TABLE ai_analysis_logs (
            id TEXT PRIMARY KEY,
            event_kind TEXT NOT NULL CHECK(event_kind IN ('request_started','response_received','request_failed','retry_scheduled','correction_requested','recovery','cancelled')),
            job_id TEXT,
            batch_id TEXT,
            target_kind TEXT CHECK(target_kind IS NULL OR target_kind IN ('managed','global_local','project_local')),
            target_key TEXT,
            target_payload_json TEXT,
            skill_name TEXT,
            request_system_prompt TEXT,
            request_user_prompt TEXT,
            raw_response TEXT,
            http_status INTEGER CHECK(http_status IS NULL OR http_status >= 0),
            input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
            output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
            total_tokens INTEGER CHECK(total_tokens IS NULL OR total_tokens >= 0),
            duration_ms INTEGER CHECK(duration_ms IS NULL OR duration_ms >= 0),
            error_code TEXT,
            error_message TEXT,
            created_at INTEGER NOT NULL
        );

        CREATE UNIQUE INDEX ux_skill_ai_analyses_target ON skill_ai_analyses(target_kind,target_key);
        CREATE INDEX ix_ai_analysis_batches_status_created ON ai_analysis_batches(status,created_at,id);
        CREATE UNIQUE INDEX ux_ai_analysis_jobs_batch_ordinal ON ai_analysis_jobs(batch_id,ordinal);
        CREATE INDEX ix_ai_analysis_jobs_claim ON ai_analysis_jobs(status,next_retry_at,priority DESC,created_at,batch_id,ordinal,id);
        CREATE UNIQUE INDEX ux_ai_analysis_jobs_active_target ON ai_analysis_jobs(target_kind,target_key) WHERE status IN ('queued','running','retry_wait','interrupted');
        CREATE INDEX ix_ai_analysis_jobs_target_updated ON ai_analysis_jobs(target_kind,target_key,updated_at DESC,id DESC);
        CREATE INDEX ix_ai_analysis_logs_created ON ai_analysis_logs(created_at,id);
        CREATE INDEX ix_ai_analysis_logs_job ON ai_analysis_logs(job_id);
        CREATE INDEX ix_ai_analysis_logs_filters ON ai_analysis_logs(event_kind,error_code,batch_id,created_at DESC,id DESC);
        ",
    )?;
    Ok(())
}

// ── Helpers ──

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    // Validate identifiers to prevent SQL injection if call sites ever change.
    validate_identifier(table)?;
    validate_identifier(column)?;

    if !has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn validate_identifier(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("Invalid SQL identifier: {}", name);
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrate_to_version(conn: &Connection, target_version: u32) {
        // Tests build historical databases through the production steps so a
        // v7 fixture cannot silently drift from the real released structure.
        for version in 0..target_version {
            conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
            migrate_step(conn, version).unwrap();
            conn.pragma_update(None, "user_version", version + 1)
                .unwrap();
            conn.execute_batch("COMMIT").unwrap();
        }
    }

    fn schema_object_exists(conn: &Connection, object_type: &str, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2)",
            (object_type, name),
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_fresh_database_migrates_to_latest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        run_migrations(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_VERSION);

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"skills".to_string()));
        assert!(tables.contains(&"skill_targets".to_string()));
        assert!(tables.contains(&"scenarios".to_string()));
        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"skill_tags".to_string()));
        assert!(tables.contains(&"scenario_skill_tools".to_string()));
        assert!(tables.contains(&"audit_log".to_string()));
        assert!(tables.contains(&"skill_ai_analyses".to_string()));
        assert!(tables.contains(&"ai_analysis_batches".to_string()));
        assert!(tables.contains(&"ai_analysis_jobs".to_string()));
        assert!(tables.contains(&"ai_analysis_logs".to_string()));
    }

    #[test]
    fn test_real_v7_database_upgrades_to_v8_with_named_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate_to_version(&conn, 7);

        run_migrations(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);

        let expected_indexes = [
            "ux_skill_ai_analyses_target",
            "ix_ai_analysis_batches_status_created",
            "ux_ai_analysis_jobs_batch_ordinal",
            "ix_ai_analysis_jobs_claim",
            "ux_ai_analysis_jobs_active_target",
            "ix_ai_analysis_jobs_target_updated",
            "ix_ai_analysis_logs_created",
            "ix_ai_analysis_logs_job",
            "ix_ai_analysis_logs_filters",
        ];
        for index in expected_indexes {
            assert!(
                schema_object_exists(&conn, "index", index),
                "missing {index}"
            );
        }

        let active_index_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'ux_ai_analysis_jobs_active_target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(active_index_sql
            .contains("WHERE status IN ('queued','running','retry_wait','interrupted')"));

        // The database must reject identity variants outside the frozen
        // three-way union even when a future caller bypasses Rust enums.
        let invalid_target = conn.execute(
            "INSERT INTO skill_ai_analyses (
                id,target_kind,target_key,target_payload_json,skill_name,source_hash,
                schema_version,prompt_version,output_language,one_line,result_json,
                provider,model,analyzed_at,created_at,updated_at
             ) VALUES ('bad','other','[]','{}','bad','hash',1,'p1','en','bad','{}','custom','m',1,1,1)",
            [],
        );
        assert!(invalid_target.is_err());
    }

    #[test]
    fn test_v8_migration_failure_rolls_back_entire_step() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate_to_version(&conn, 7);
        // This deliberate collision happens after two v8 tables are created,
        // proving that an intermediate failure rolls back the whole step.
        conn.execute_batch("CREATE TABLE ai_analysis_jobs (sentinel TEXT);")
            .unwrap();

        let error = run_migrations(&conn).unwrap_err();
        assert!(error
            .to_string()
            .contains("migration from version 7 to 8 failed"));

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 7);
        assert!(!schema_object_exists(&conn, "table", "skill_ai_analyses"));
        assert!(!schema_object_exists(&conn, "table", "ai_analysis_batches"));
        assert!(!schema_object_exists(&conn, "table", "ai_analysis_logs"));
        assert!(!schema_object_exists(
            &conn,
            "index",
            "ux_skill_ai_analyses_target"
        ));
        assert!(has_column(&conn, "ai_analysis_jobs", "sentinel").unwrap());
    }

    #[test]
    fn every_post_begin_failure_rolls_back_and_leaves_connection_reusable() {
        #[derive(Clone, Copy)]
        enum FailureStage {
            Migrate,
            UserVersion,
            Commit,
        }

        for stage in [
            FailureStage::Migrate,
            FailureStage::UserVersion,
            FailureStage::Commit,
        ] {
            let conn = Connection::open_in_memory().unwrap();
            let error = run_migration_transaction(
                &conn,
                7,
                |connection| {
                    connection.execute_batch("CREATE TABLE rollback_probe (id INTEGER);")?;
                    if matches!(stage, FailureStage::Migrate) {
                        bail!("forced migrate_step failure");
                    }
                    Ok(())
                },
                |_connection| {
                    if matches!(stage, FailureStage::UserVersion) {
                        bail!("forced user_version failure");
                    }
                    Ok(())
                },
                |connection| {
                    if matches!(stage, FailureStage::Commit) {
                        bail!("forced COMMIT failure");
                    }
                    connection.execute_batch("COMMIT")?;
                    Ok(())
                },
            )
            .unwrap_err();

            assert!(error
                .to_string()
                .contains("migration from version 7 to 8 failed"));
            assert!(conn.is_autocommit());
            assert!(!schema_object_exists(&conn, "table", "rollback_probe"));
            // A fresh transaction succeeding proves no failed path left a
            // transaction attached to the reusable application connection.
            conn.execute_batch("BEGIN IMMEDIATE; CREATE TABLE reuse_probe (id INTEGER); COMMIT;")
                .unwrap();
        }
    }

    #[test]
    fn test_idempotent_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        run_migrations(&conn).unwrap();
        // Running again should be a no-op
        run_migrations(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_VERSION);
    }

    #[test]
    fn test_pre_migration_database_upgrades() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        // Simulate a pre-migration database: create skills table without newer columns
        conn.execute_batch(
            "
            CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                source_type TEXT NOT NULL,
                source_ref TEXT,
                source_revision TEXT,
                central_path TEXT NOT NULL UNIQUE,
                content_hash TEXT,
                enabled INTEGER DEFAULT 1,
                created_at INTEGER,
                updated_at INTEGER,
                status TEXT DEFAULT 'ok'
            );
            CREATE TABLE scenarios (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT,
                sort_order INTEGER DEFAULT 0,
                created_at INTEGER,
                updated_at INTEGER
            );
            ",
        )
        .unwrap();

        // user_version is 0 (default), so migration should run
        run_migrations(&conn).unwrap();

        // Verify new columns were added
        assert!(has_column(&conn, "skills", "source_ref_resolved").unwrap());
        assert!(has_column(&conn, "skills", "source_subpath").unwrap());
        assert!(has_column(&conn, "skills", "source_branch").unwrap());
        assert!(has_column(&conn, "skills", "remote_revision").unwrap());
        assert!(has_column(&conn, "skills", "update_status").unwrap());
        assert!(has_column(&conn, "skills", "last_checked_at").unwrap());
        assert!(has_column(&conn, "skills", "last_check_error").unwrap());
        assert!(has_column(&conn, "scenarios", "icon").unwrap());

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_VERSION);
    }

    #[test]
    fn test_v1_database_upgrades_to_v2() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        conn.execute_batch(
            "
            CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                source_type TEXT NOT NULL,
                source_ref TEXT,
                source_ref_resolved TEXT,
                source_subpath TEXT,
                source_branch TEXT,
                source_revision TEXT,
                remote_revision TEXT,
                central_path TEXT NOT NULL UNIQUE,
                content_hash TEXT,
                enabled INTEGER DEFAULT 1,
                created_at INTEGER,
                updated_at INTEGER,
                status TEXT DEFAULT 'ok',
                update_status TEXT DEFAULT 'unknown',
                last_checked_at INTEGER,
                last_check_error TEXT
            );
            CREATE TABLE scenarios (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT,
                icon TEXT,
                sort_order INTEGER DEFAULT 0,
                created_at INTEGER,
                updated_at INTEGER
            );
            CREATE TABLE scenario_skills (
                scenario_id TEXT NOT NULL REFERENCES scenarios(id) ON DELETE CASCADE,
                skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
                added_at INTEGER,
                PRIMARY KEY(scenario_id, skill_id)
            );
            CREATE TABLE skill_targets (
                id TEXT PRIMARY KEY,
                skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
                tool TEXT NOT NULL,
                target_path TEXT NOT NULL,
                mode TEXT NOT NULL,
                status TEXT DEFAULT 'ok',
                synced_at INTEGER,
                last_error TEXT,
                UNIQUE(skill_id, tool)
            );
            PRAGMA user_version = 1;
            ",
        )
        .unwrap();

        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "scenario_skill_tools", "enabled").unwrap());
        assert!(has_column(&conn, "skill_targets", "source_hash").unwrap());

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_VERSION);
    }

    #[test]
    fn test_newer_schema_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", LATEST_VERSION + 1)
            .unwrap();

        let err = run_migrations(&conn).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("newer than this app supports"),
            "unexpected error: {msg}"
        );
    }
}
