use std::time::{SystemTime, UNIX_EPOCH};

use regex::{Captures, Regex};
use rusqlite::params;

use crate::core::skill_store::SkillStore;

use super::command_error;
use super::config::retention_days_for_cleanup;
use super::repository::AiRepository;
use super::types::{AiCommandError, AiErrorCode, AiErrorKind, AiLogRecord};

const MAX_ERROR_MESSAGE_CHARS: usize = 4_096;
const MAX_RAW_RESPONSE_LOG_BYTES: usize = 65_536;
const RAW_RESPONSE_TRUNCATION_MARKER: &str = "\n[TRUNCATED]";

/// Repository code can inspect this record but cannot construct one; the
/// private field makes this module the only gate from raw text to persistence.
pub(super) struct SanitizedAiLogRecord(AiLogRecord);

impl SanitizedAiLogRecord {
    pub(super) fn as_record(&self) -> &AiLogRecord {
        &self.0
    }
}

pub fn save_log(
    store: &SkillStore,
    record: AiLogRecord,
    current_api_key: Option<&str>,
) -> Result<(), AiCommandError> {
    let sanitized = sanitized_record(record, current_api_key);
    AiRepository::new(store)
        .insert_log(&sanitized)
        .map_err(|_| log_storage_error("save"))
}

/// The only constructor of `SanitizedAiLogRecord`: raw semantic text crosses
/// the redaction gate here, and repository code cannot build one itself.
pub(super) fn sanitized_record(
    mut record: AiLogRecord,
    current_api_key: Option<&str>,
) -> SanitizedAiLogRecord {
    // All untrusted semantic text passes exact-key replacement before generic
    // redaction; callers never hand a raw record directly to the repository.
    sanitize_optional(&mut record.skill_name, current_api_key, false);
    sanitize_optional(&mut record.target_key, current_api_key, false);
    sanitize_optional(&mut record.target_payload_json, current_api_key, false);
    sanitize_optional(&mut record.request_system_prompt, current_api_key, false);
    sanitize_optional(&mut record.request_user_prompt, current_api_key, false);
    if let Some(raw_response) = &mut record.raw_response {
        // 诊断响应可能来自不受信任的服务商；先统一脱敏再按 UTF-8 边界限长，
        // 避免排查日志意外保存认证信息或无限膨胀数据库。
        *raw_response = sanitize_raw_response(raw_response, current_api_key);
    }
    sanitize_optional(&mut record.error_message, current_api_key, true);
    SanitizedAiLogRecord(record)
}

pub fn cleanup_expired_logs_on_startup(store: &SkillStore) -> Result<usize, AiCommandError> {
    let retention_days = retention_days_for_cleanup(store)?;
    cleanup_expired_logs_at(store, retention_days, now_millis())
}

pub fn cleanup_expired_logs(
    store: &SkillStore,
    retention_days: u16,
) -> Result<usize, AiCommandError> {
    cleanup_expired_logs_at(store, retention_days, now_millis())
}

pub fn clear_logs(store: &SkillStore) -> Result<usize, AiCommandError> {
    store
        .with_ai_transaction(|transaction| {
            transaction
                .execute("DELETE FROM ai_analysis_logs", [])
                .map_err(anyhow::Error::from)
        })
        // Never forward SQLite text here: cleanup errors must not accidentally
        // include values from a failed prompt-bearing database statement.
        .map_err(|_| log_storage_error("clear"))
}

pub fn sanitize_log_text(value: &str, current_api_key: Option<&str>) -> String {
    // Exact replacement happens first because a valid key may contain spaces
    // or punctuation that generic credential patterns cannot recognize.
    let mut sanitized = match current_api_key.filter(|key| !key.is_empty()) {
        Some(api_key) => value.replace(api_key, "[REDACTED]"),
        None => value.to_string(),
    };

    let header_pattern =
        Regex::new(r"(?i)(authorization|proxy-authorization|cookie|set-cookie)\s*:\s*[^\r\n]*")
            .expect("static credential header regex must compile");
    sanitized = header_pattern
        .replace_all(&sanitized, "$1: [REDACTED]")
        .into_owned();

    // Escaped-string branches consume the entire JSON/diagnostic value so an
    // embedded quote cannot expose its tail. Encoded delimiters are matched
    // directly without decoding or allocating an unbounded second payload.
    let named_secret_pattern = Regex::new(
        r#"(?i)(["']?(?:api(?:[_-]|%5f|%2d)?key|access(?:[_-]|%5f|%2d)?token|token|password|authorization|proxy(?:[_-]|%5f|%2d)?authorization|cookie|set(?:[_-]|%5f|%2d)?cookie)["']?\s*(?::|=|%3a|%3d)\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^&\s,;}\]]+)"#,
    )
    .expect("static named-secret regex must compile");
    sanitized = named_secret_pattern
        .replace_all(&sanitized, |captures: &Captures<'_>| {
            let prefix = captures.get(1).map_or("", |value| value.as_str());
            let original = captures.get(2).map_or("", |value| value.as_str());
            let quote = match (original.as_bytes().first(), original.as_bytes().last()) {
                (Some(b'"'), Some(b'"')) => "\"",
                (Some(b'\''), Some(b'\'')) => "'",
                _ => "",
            };
            format!("{prefix}{quote}[REDACTED]{quote}")
        })
        .into_owned();

    let bearer_pattern =
        Regex::new(r"(?i)\bbearer\s+[^\s,;]+").expect("static bearer regex must compile");
    bearer_pattern
        .replace_all(&sanitized, "Bearer [REDACTED]")
        .into_owned()
}

pub fn sanitize_error_message(value: &str, current_api_key: Option<&str>) -> String {
    sanitize_log_text(value, current_api_key)
        .chars()
        .take(MAX_ERROR_MESSAGE_CHARS)
        .collect()
}

fn sanitize_raw_response(value: &str, current_api_key: Option<&str>) -> String {
    let sanitized = sanitize_log_text(value, current_api_key);
    if sanitized.len() <= MAX_RAW_RESPONSE_LOG_BYTES {
        return sanitized;
    }

    let limit = MAX_RAW_RESPONSE_LOG_BYTES.saturating_sub(RAW_RESPONSE_TRUNCATION_MARKER.len());
    let boundary = sanitized
        .char_indices()
        .take_while(|(index, character)| index.saturating_add(character.len_utf8()) <= limit)
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    format!(
        "{}{}",
        &sanitized[..boundary],
        RAW_RESPONSE_TRUNCATION_MARKER
    )
}

fn sanitize_optional(value: &mut Option<String>, current_api_key: Option<&str>, is_error: bool) {
    if let Some(text) = value {
        *text = if is_error {
            sanitize_error_message(text, current_api_key)
        } else {
            sanitize_log_text(text, current_api_key)
        };
    }
}

fn cleanup_expired_logs_at(
    store: &SkillStore,
    retention_days: u16,
    now_millis: i64,
) -> Result<usize, AiCommandError> {
    if !(1..=3650).contains(&retention_days) {
        return Err(command_error(
            AiErrorKind::Configuration,
            AiErrorCode::InvalidConfig,
            "The AI log retention period is invalid.",
            false,
        ));
    }
    let retention_millis = i64::from(retention_days) * 86_400_000;
    let cutoff = now_millis.saturating_sub(retention_millis);
    // The table name is fixed and the cutoff is bound, ensuring this privacy
    // cleanup can never widen into other application data.
    store
        .with_ai_transaction(|transaction| {
            transaction
                .execute(
                    "DELETE FROM ai_analysis_logs WHERE created_at < ?1",
                    params![cutoff],
                )
                .map_err(anyhow::Error::from)
        })
        .map_err(|_| log_storage_error("clean up"))
}

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn log_storage_error(operation: &str) -> AiCommandError {
    command_error(
        AiErrorKind::Storage,
        AiErrorCode::Db,
        format!("Unable to {operation} AI analysis logs."),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ai::config::AI_CONFIG_SETTING_KEY;
    use crate::core::ai::types::{AiLogEventKind, AiTargetKind};
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, SkillStore) {
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("logs.db")).unwrap();
        (directory, store)
    }

    fn insert_log(store: &SkillStore, id: &str, created_at: i64) {
        store
            .with_ai_transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO ai_analysis_logs (id,event_kind,created_at) VALUES (?1,'recovery',?2)",
                    params![id, created_at],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn log_count(store: &SkillStore) -> i64 {
        store
            .with_ai_connection(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM ai_analysis_logs", [], |row| {
                        row.get(0)
                    })
                    .map_err(anyhow::Error::from)
            })
            .unwrap()
    }

    #[test]
    fn cleanup_removes_only_logs_older_than_thirty_days() {
        let (_directory, store) = store();
        let now = 50 * 86_400_000_i64;
        insert_log(&store, "expired", now - 31 * 86_400_000);
        insert_log(&store, "boundary", now - 30 * 86_400_000);
        insert_log(&store, "fresh", now - 1);
        assert_eq!(cleanup_expired_logs_at(&store, 30, now).unwrap(), 1);
        assert_eq!(log_count(&store), 2);
    }

    #[test]
    fn corrupt_config_still_uses_privacy_default() {
        let (_directory, store) = store();
        store
            .set_setting(AI_CONFIG_SETTING_KEY, "corrupt credential-like text")
            .unwrap();
        assert_eq!(retention_days_for_cleanup(&store).unwrap(), 30);
    }

    #[test]
    fn clear_is_scoped_to_ai_logs_and_reports_count() {
        let (_directory, store) = store();
        insert_log(&store, "one", 1);
        insert_log(&store, "two", 2);
        assert_eq!(clear_logs(&store).unwrap(), 2);
        assert_eq!(clear_logs(&store).unwrap(), 0);
    }

    #[test]
    fn exact_key_is_removed_before_general_redaction() {
        let api_key = "placeholder credential with spaces";
        let raw = format!(
            "payload={api_key}\nAuthorization: Bearer another-value\napi_key=third-value\nCookie: sid=fourth"
        );
        let sanitized = sanitize_log_text(&raw, Some(api_key));
        assert!(!sanitized.contains(api_key));
        assert!(!sanitized.contains("another-value"));
        assert!(!sanitized.contains("third-value"));
        assert!(!sanitized.contains("sid=fourth"));
    }

    #[test]
    fn general_redaction_covers_json_headers_query_and_form_values() {
        let raw = r#"{"api_key":"json-secret","token":'quoted-token-secret',"password":"quoted-password-secret","authorization":"Bearer json-auth-secret","proxy-authorization":"Basic json-proxy-secret","cookie":"sid=json-cookie-secret","set-cookie":"sid=json-set-cookie-secret"}
Authorization: Bearer header-auth-secret
Proxy-Authorization: Basic header-proxy-secret
Cookie: sid=header-cookie-secret
Set-Cookie: sid=header-set-cookie-secret
https://example.invalid/?api_key=query-secret&token=query-token-secret
password=form-password-secret&access_token=form-token-secret"#;
        let sanitized = sanitize_log_text(raw, None);
        for secret in [
            "json-secret",
            "quoted-token-secret",
            "quoted-password-secret",
            "json-auth-secret",
            "json-proxy-secret",
            "json-cookie-secret",
            "json-set-cookie-secret",
            "header-auth-secret",
            "header-proxy-secret",
            "header-cookie-secret",
            "header-set-cookie-secret",
            "query-secret",
            "query-token-secret",
            "form-password-secret",
            "form-token-secret",
        ] {
            assert!(!sanitized.contains(secret), "secret survived: {secret}");
        }
    }

    #[test]
    fn redaction_consumes_escaped_quotes_and_percent_encoded_sensitive_keys() {
        let raw = r#"{"password":"abc\\\"json-tail-secret","token":'abc\\\'single-tail-secret'}
api%5Fkey%3Dencoded-api-secret&access%5Ftoken=encoded-access-secret
proxy%2Dauthorization%3ABasic%20encoded-proxy-secret&set%2Dcookie=encoded-cookie-secret"#;
        let sanitized = sanitize_log_text(raw, None);
        for secret in [
            "json-tail-secret",
            "single-tail-secret",
            "encoded-api-secret",
            "encoded-access-secret",
            "encoded-proxy-secret",
            "encoded-cookie-secret",
        ] {
            assert!(!sanitized.contains(secret), "secret survived: {secret}");
        }
        assert!(sanitized.matches("[REDACTED]").count() >= 6);
    }

    #[test]
    fn save_path_redacts_before_repository_insert() {
        let (_directory, store) = store();
        let api_key = "placeholder credential with punctuation!@#";
        let record = AiLogRecord {
            id: "redacted-log".into(),
            event_kind: AiLogEventKind::RequestFailed,
            job_id: None,
            batch_id: None,
            target_kind: Some(AiTargetKind::Managed),
            target_key: Some("[\"skill\"]".into()),
            target_payload_json: Some("{\"kind\":\"managed\"}".into()),
            skill_name: Some(format!("Skill {api_key}")),
            request_system_prompt: Some(
                r#"{"api_key":"persisted-json-secret","password":"prefix\\\"persisted-json-tail-secret"}"#.into(),
            ),
            request_user_prompt: Some(
                r#"token='prefix\\\'persisted-single-tail-secret'&api%5Fkey%3Dpersisted-encoded-api-secret&access%5Ftoken=persisted-encoded-access-secret"#.into(),
            ),
            raw_response: Some(
                "Proxy-Authorization: Basic persisted-proxy-secret\nCookie: sid=persisted-cookie-secret\nproxy%2Dauthorization%3ABasic%20persisted-encoded-proxy-secret&set%2Dcookie=persisted-encoded-cookie-secret".into(),
            ),
            http_status: Some(401),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            duration_ms: Some(1),
            error_code: Some("http_auth".into()),
            error_message: Some(
                r#"{"authorization":"Bearer persisted-auth-secret","set-cookie":"sid=persisted-set-cookie-secret"}"#
                    .into(),
            ),
            created_at: 1,
        };
        save_log(&store, record, Some(api_key)).unwrap();
        store
            .with_ai_connection(|connection| {
                let stored: (String, String, String, String, String) = connection.query_row(
                    "SELECT skill_name,request_system_prompt,request_user_prompt,raw_response,error_message FROM ai_analysis_logs WHERE id='redacted-log'",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )?;
                let persisted = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    stored.0, stored.1, stored.2, stored.3, stored.4
                );
                assert!(persisted.contains("[REDACTED]"));
                for secret in [
                    api_key,
                    "persisted-json-secret",
                    "persisted-json-tail-secret",
                    "persisted-single-tail-secret",
                    "persisted-encoded-api-secret",
                    "persisted-encoded-access-secret",
                    "persisted-proxy-secret",
                    "persisted-cookie-secret",
                    "persisted-encoded-proxy-secret",
                    "persisted-encoded-cookie-secret",
                    "persisted-auth-secret",
                    "persisted-set-cookie-secret",
                ] {
                    assert!(!persisted.contains(secret), "secret persisted: {secret}");
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn raw_response_is_redacted_and_limited_without_breaking_utf8() {
        let api_key = "response-secret";
        let raw = format!(
            "Authorization: Bearer {api_key}\n{}",
            "多".repeat(MAX_RAW_RESPONSE_LOG_BYTES)
        );
        let sanitized = sanitize_raw_response(&raw, Some(api_key));

        assert!(!sanitized.contains(api_key));
        assert!(sanitized.contains("Authorization: [REDACTED]"));
        assert!(sanitized.ends_with(RAW_RESPONSE_TRUNCATION_MARKER));
        assert!(sanitized.len() <= MAX_RAW_RESPONSE_LOG_BYTES);
    }
}
