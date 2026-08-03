//! AI-only main-document collector.
//!
//! Skill text is untrusted input, and the frozen contract forbids reusing the
//! three detail-page readers (they recurse or canonicalize-then-reopen). This
//! collector re-locates each target from authoritative scan data, then reads
//! only first-level candidates through descriptor-relative no-follow opens so
//! a symlink swap between check and read cannot leak an unrelated file.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::core::central_repo;
use crate::core::project_scanner::{self, AgentSkillConfig};
use crate::core::skill_store::{ProjectRecord, SkillStore};
use crate::core::tool_adapters;

use super::command_error;
use super::types::{AiCommandError, AiErrorCode, AiErrorKind, AiTargetRef};

pub const MAX_DOCUMENT_BYTES: usize = 1_048_576;
const DOCUMENT_CANDIDATES: [&str; 4] = ["SKILL.md", "skill.md", "CLAUDE.md", "README.md"];

/// The central skills root is a process-global override; any test that
/// exercises the collector must hold this lock so parallel tests cannot
/// canonicalize against each other's temporary roots.
#[cfg(test)]
pub(crate) static CENTRAL_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A successfully collected first-level main document. `bytes` and `content`
/// are the same payload; the hash is computed over the original UTF-8 bytes so
/// preview and execution compare identical source material.
pub struct CollectedDocument {
    pub skill_name: String,
    pub document_filename: String,
    pub content: String,
    pub bytes: Vec<u8>,
    pub source_hash: String,
    pub character_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOutcome {
    Ready,
    NoDocument,
    Unreadable { error_code: AiErrorCode },
}

/// Everything needed to read one target's main document without trusting the
/// frontend-supplied path beyond exact scan-result matching.
struct LocatedSkill {
    skill_name: String,
    skill_dir: PathBuf,
    /// Canonicalized allowed roots; the final skill directory must resolve
    /// inside one of them.
    allowed_roots: Vec<PathBuf>,
    /// Canonical skill directories of every managed skill, used to prove a
    /// center-root symlink points at an app-registered skill.
    managed_central_dirs: BTreeSet<PathBuf>,
}

/// Public entry point: locate the target and read its first candidate file.
/// Locate/read failures map to a structured outcome so previews can report
/// unreadable items without aborting the whole scan.
pub fn collect_document(
    store: &SkillStore,
    target: &AiTargetRef,
) -> (DocumentOutcome, Option<CollectedDocument>) {
    let located = match locate_skill(store, target) {
        Ok(located) => located,
        Err(error) => {
            return (
                DocumentOutcome::Unreadable {
                    error_code: error.code,
                },
                None,
            );
        }
    };
    read_first_candidate(&located)
}

/// Resolve the skill directory and allowed roots for one target.
fn locate_skill(store: &SkillStore, target: &AiTargetRef) -> Result<LocatedSkill, AiCommandError> {
    let managed_central_dirs = managed_central_dirs(store)?;
    let central_root = canonical_optional(&central_repo::skills_dir());

    match target {
        AiTargetRef::Managed { skill_id } => {
            let skill = store
                .get_skill_by_id(skill_id)
                .map_err(db_error)?
                .ok_or_else(|| not_found("managed skill"))?;
            let skill_dir = PathBuf::from(&skill.central_path);
            let Some(canonical_dir) = canonical_optional(&skill_dir) else {
                return Err(unreadable());
            };
            let Some(root) = central_root.clone() else {
                return Err(unreadable());
            };
            // A managed skill must live inside the canonical center skills root.
            if !canonical_dir.starts_with(&root) {
                return Err(unsafe_path());
            }
            Ok(LocatedSkill {
                skill_name: skill.name,
                skill_dir,
                allowed_roots: vec![root],
                managed_central_dirs,
            })
        }
        AiTargetRef::GlobalLocal {
            agent_key,
            relative_path,
        } => {
            validate_relative_path(relative_path)?;
            let adapter = tool_adapters::all_tool_adapters(store)
                .into_iter()
                .find(|adapter| adapter.key == *agent_key)
                .ok_or_else(|| invalid_target("unknown agent"))?;
            let skills = project_scanner::read_linked_workspace_skills(
                &adapter.skills_dir(),
                None,
                &adapter.key,
                &adapter.display_name,
                adapter.recursive_scan,
            );
            let mut matches: Vec<_> = skills
                .into_iter()
                .filter(|skill| skill.agent == *agent_key && skill.relative_path == *relative_path)
                .collect();
            let skill = match matches.len() {
                0 => return Err(not_found("global local skill")),
                1 => matches.remove(0),
                // The global adapter has no disabled root, but a duplicate scan
                // entry is still ambiguous and must never silently pick one.
                _ => return Err(ambiguous_target()),
            };
            let mut allowed_roots = Vec::new();
            if let Some(root) = canonical_optional(&adapter.skills_dir()) {
                allowed_roots.push(root);
            }
            if let Some(root) = central_root {
                allowed_roots.push(root);
            }
            Ok(LocatedSkill {
                skill_name: skill.name,
                skill_dir: PathBuf::from(&skill.path),
                allowed_roots,
                managed_central_dirs,
            })
        }
        AiTargetRef::ProjectLocal {
            project_id,
            agent_key,
            relative_path,
        } => {
            validate_relative_path(relative_path)?;
            let record = store
                .get_project_by_id(project_id)
                .map_err(db_error)?
                .ok_or_else(|| not_found("workspace"))?;
            let (skills, skill_dir, mut allowed_roots) =
                locate_project_skill(store, &record, agent_key)?;
            let mut matches: Vec<_> = skills
                .into_iter()
                .filter(|skill| skill.agent == *agent_key && skill.relative_path == *relative_path)
                .collect();
            let skill = match matches.len() {
                0 => return Err(not_found("project skill")),
                1 => matches.remove(0),
                // Enabled and disabled roots may both expose the same relative
                // path; the contract rejects this instead of choosing one.
                _ => return Err(ambiguous_target()),
            };
            if let Some(root) = central_root {
                allowed_roots.push(root);
            }
            Ok(LocatedSkill {
                skill_name: skill.name,
                skill_dir: skill_dir.unwrap_or_else(|| PathBuf::from(&skill.path)),
                allowed_roots,
                managed_central_dirs,
            })
        }
    }
}

/// Returns the authoritative scan result plus the candidate skill directory for
/// a project target. The frontend path is only a lookup hint; the scanner's own
/// `agent + relative_path` is the identity source per the frozen contract.
fn locate_project_skill(
    store: &SkillStore,
    record: &ProjectRecord,
    agent_key: &str,
) -> Result<
    (
        Vec<project_scanner::ProjectSkillInfo>,
        Option<PathBuf>,
        Vec<PathBuf>,
    ),
    AiCommandError,
> {
    if record.workspace_type == "linked" {
        if linked_agent_key(record) != agent_key {
            return Err(invalid_target("linked workspace agent mismatch"));
        }
        let disabled = record.disabled_path.as_deref().map(PathBuf::from);
        let skills = project_scanner::read_linked_workspace_skills(
            Path::new(&record.path),
            disabled.as_deref(),
            &linked_agent_key(record),
            &linked_agent_name(record),
            true,
        );
        let mut roots = vec![PathBuf::from(&record.path)];
        if let Some(disabled) = disabled {
            roots.push(disabled);
        }
        return Ok((skills, None, roots));
    }

    let adapter = tool_adapters::all_tool_adapters(store)
        .into_iter()
        .find(|adapter| adapter.key == *agent_key)
        .ok_or_else(|| invalid_target("unknown agent"))?;
    let configs = agent_scan_configs(store);
    let skills = project_scanner::read_project_skills(Path::new(&record.path), &configs);
    let project_dir = adapter.project_relative_skills_dir().to_string();
    let skills_root = Path::new(&record.path).join(&project_dir);
    let disabled_root = Path::new(&record.path).join(format!("{project_dir}-disabled"));
    Ok((skills, None, vec![skills_root, disabled_root]))
}

/// Lexical validation shared by global/project relative paths: only `/`
/// separated normal components, no absolute path, backslash, `.`/`..`, NUL or
/// control characters. Alias/duplicate components are rejected by the exact
/// scan-result match below, never normalized into a second identity.
fn validate_relative_path(relative_path: &str) -> Result<(), AiCommandError> {
    if relative_path.trim().is_empty() {
        return Err(invalid_target("empty relative path"));
    }
    // Backslash is never a valid separator in the frozen identity grammar even
    // on Unix, where Path does not treat it specially.
    if relative_path.contains('\\') {
        return Err(invalid_target("invalid relative path"));
    }
    // `/`-separated grammar requires every fragment to be non-empty; Path
    // silently collapses repeated separators, so split explicitly.
    if relative_path
        .split('/')
        .any(|fragment| fragment.is_empty() || fragment == "." || fragment == "..")
    {
        return Err(invalid_target("invalid relative path"));
    }
    let mut saw_component = false;
    for component in Path::new(relative_path).components() {
        let Component::Normal(value) = component else {
            return Err(invalid_target("invalid relative path"));
        };
        let value = value
            .to_str()
            .ok_or_else(|| invalid_target("non-UTF-8 path"))?;
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains('\0')
            || value.chars().any(char::is_control)
        {
            return Err(invalid_target("invalid relative path component"));
        }
        saw_component = true;
    }
    if !saw_component {
        return Err(invalid_target("invalid relative path"));
    }
    Ok(())
}

/// Read the first existing candidate through an already-open no-follow handle,
/// verifying the handle identity before and after the bounded read.
#[cfg(unix)]
fn read_first_candidate(located: &LocatedSkill) -> (DocumentOutcome, Option<CollectedDocument>) {
    let Some(canonical_target) = canonical_optional(&located.skill_dir) else {
        return unreadable_outcome();
    };
    let roots: Vec<PathBuf> = located
        .allowed_roots
        .iter()
        .filter_map(|path| canonical_optional(path))
        .collect();
    let Some(root) = roots.iter().find(|root| canonical_target.starts_with(root)) else {
        return unsafe_outcome();
    };
    // Center-root rule: a skill resolving into the central skills root is only
    // acceptable when it is exactly an app-registered managed skill directory.
    if let Some(central_canonical) = canonical_optional(&central_repo::skills_dir()) {
        if canonical_target.starts_with(&central_canonical)
            && !located.managed_central_dirs.contains(&canonical_target)
        {
            return unsafe_outcome();
        }
    }

    // Open the canonical root, then descend component-by-component with
    // O_NOFOLLOW so a swap between canonicalize and open is still caught.
    let Some(root_fd) = open_dir_no_follow(root) else {
        return unreadable_outcome();
    };
    let relative = canonical_target.strip_prefix(root).unwrap_or(Path::new(""));
    let mut current_fd = root_fd;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return unsafe_outcome();
        };
        let Ok(cstring) = std::ffi::CString::new(name.to_string_lossy().as_bytes()) else {
            return unsafe_outcome();
        };
        match openat_dir(&current_fd, &cstring) {
            Ok(next) => {
                current_fd = next;
            }
            Err(_) => {
                return unsafe_outcome();
            }
        }
    }

    for candidate in DOCUMENT_CANDIDATES {
        let Ok(cstring) = std::ffi::CString::new(candidate) else {
            continue;
        };
        let Ok(file_fd) = openat_file(&current_fd, &cstring) else {
            // Missing or symlink candidates are skipped; a symlink document
            // must never be followed into an unverified location.
            continue;
        };
        match read_bounded_file(file_fd) {
            Ok(bytes) => {
                let Ok(content) = String::from_utf8(bytes.clone()) else {
                    return (
                        DocumentOutcome::Unreadable {
                            error_code: AiErrorCode::InvalidUtf8,
                        },
                        None,
                    );
                };
                let source_hash = hex::encode(Sha256::digest(&bytes));
                let character_count = content.chars().count() as i64;
                return (
                    DocumentOutcome::Ready,
                    Some(CollectedDocument {
                        skill_name: located.skill_name.clone(),
                        document_filename: candidate.to_string(),
                        content,
                        bytes,
                        source_hash,
                        character_count,
                    }),
                );
            }
            Err(error) => return (DocumentOutcome::Unreadable { error_code: error }, None),
        }
    }
    drop(current_fd);
    (DocumentOutcome::NoDocument, None)
}

#[cfg(windows)]
fn read_first_candidate(located: &LocatedSkill) -> (DocumentOutcome, Option<CollectedDocument>) {
    // Windows conservative path: the std library cannot open directories
    // descriptor-relative with no-follow, so we canonicalize the target into a
    // verified root, reject symlink/junction skill directories unless they are
    // an exact managed center dir, reject symlink candidates, and re-stat the
    // file after reading. Handle-level reparse validation is pending a Windows
    // environment and is tracked as a known limitation.
    let Some(canonical_target) = canonical_optional(&located.skill_dir) else {
        return unreadable_outcome();
    };
    let roots: Vec<PathBuf> = located
        .allowed_roots
        .iter()
        .filter_map(canonical_optional)
        .collect();
    let Some(root) = roots.iter().find(|root| canonical_target.starts_with(root)) else {
        return unsafe_outcome();
    };
    if canonical_target.starts_with(&central_repo::skills_dir())
        && !located.managed_central_dirs.contains(&canonical_target)
    {
        return unsafe_outcome();
    }
    let _ = root;

    for candidate in DOCUMENT_CANDIDATES {
        let file_path = canonical_target.join(candidate);
        let Ok(meta) = std::fs::symlink_metadata(&file_path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let before = meta.len();
        let mut file = match std::fs::File::open(&file_path) {
            Ok(file) => file,
            Err(_) => return unreadable_outcome(),
        };
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 16_384];
        loop {
            match std::io::Read::read(&mut file, &mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if bytes.len().saturating_add(read) > MAX_DOCUMENT_BYTES {
                        return (
                            DocumentOutcome::Unreadable {
                                error_code: AiErrorCode::DocumentTooLarge,
                            },
                            None,
                        );
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                Err(_) => return unreadable_outcome(),
            }
        }
        // Re-check the on-disk identity after reading to detect replacement.
        let Ok(after) = std::fs::symlink_metadata(&file_path) else {
            return unsafe_outcome();
        };
        if before != after.len() {
            return unsafe_outcome();
        }
        let Ok(content) = String::from_utf8(bytes.clone()) else {
            return (
                DocumentOutcome::Unreadable {
                    error_code: AiErrorCode::InvalidUtf8,
                },
                None,
            );
        };
        let source_hash = hex::encode(Sha256::digest(&bytes));
        return (
            DocumentOutcome::Ready,
            Some(CollectedDocument {
                skill_name: located.skill_name.clone(),
                document_filename: candidate.to_string(),
                content,
                bytes,
                source_hash,
                character_count: content.chars().count() as i64,
            }),
        );
    }
    (DocumentOutcome::NoDocument, None)
}

#[cfg(unix)]
fn open_dir_no_follow(path: &Path) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let cstring = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()?;
    // The root itself is already canonicalized, so plain O_DIRECTORY is safe;
    // every descendant is opened no-follow relative to this descriptor.
    let fd = unsafe {
        libc::open(
            cstring.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn openat_dir(
    directory: &std::os::fd::OwnedFd,
    component: &std::ffi::CString,
) -> Result<std::os::fd::OwnedFd, std::io::Error> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn openat_file(
    directory: &std::os::fd::OwnedFd,
    component: &std::ffi::CString,
) -> Result<std::os::fd::OwnedFd, std::io::Error> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Read a file descriptor with a hard byte ceiling, verifying the handle's
/// dev/inode/type identity before and after so an in-place replacement during
/// the read is detected.
#[cfg(unix)]
fn read_bounded_file(file: std::os::fd::OwnedFd) -> Result<Vec<u8>, AiErrorCode> {
    use std::os::fd::AsRawFd;

    let before = fstat_or(file.as_raw_fd(), AiErrorCode::UnreadableDocument)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16_384];
    loop {
        let read = unsafe {
            libc::read(
                file.as_raw_fd(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            )
        };
        if read < 0 {
            return Err(AiErrorCode::UnreadableDocument);
        }
        let read = read as usize;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > MAX_DOCUMENT_BYTES {
            return Err(AiErrorCode::DocumentTooLarge);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let after = fstat_or(file.as_raw_fd(), AiErrorCode::UnsafePath)?;
    if before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || before.st_mode != after.st_mode
    {
        return Err(AiErrorCode::UnsafePath);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn fstat_or(fd: std::os::raw::c_int, error: AiErrorCode) -> Result<libc::stat, AiErrorCode> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if result < 0 {
        return Err(error);
    }
    Ok(unsafe { stat.assume_init() })
}

fn managed_central_dirs(store: &SkillStore) -> Result<BTreeSet<PathBuf>, AiCommandError> {
    let skills = store.get_all_skills().map_err(db_error)?;
    Ok(skills
        .into_iter()
        .filter_map(|skill| canonical_optional(Path::new(&skill.central_path)))
        .collect())
}

fn canonical_optional(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn linked_agent_key(record: &ProjectRecord) -> String {
    record
        .linked_agent_key
        .clone()
        .unwrap_or_else(|| crate::commands::projects::slugify_skill_dir_name(&record.name))
}

fn linked_agent_name(record: &ProjectRecord) -> String {
    record
        .linked_agent_name
        .clone()
        .unwrap_or_else(|| record.name.clone())
}

/// Mirror of `commands::projects::agent_skill_configs`: project scanning
/// groups adapters by their project-relative skills directory, keeping the
/// first adapter key and joining display names. Kept local so the collector
/// can re-run the exact authoritative scan without exposing a private command.
fn agent_scan_configs(store: &SkillStore) -> Vec<AgentSkillConfig> {
    let mut grouped: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for adapter in tool_adapters::all_tool_adapters(store) {
        let project_dir = adapter.project_relative_skills_dir().to_string();
        if project_dir.is_empty() {
            continue;
        }
        if let Some((_, agents)) = grouped
            .iter_mut()
            .find(|(directory, _)| *directory == project_dir)
        {
            agents.push((adapter.key.clone(), adapter.display_name.clone()));
        } else {
            grouped.push((
                project_dir,
                vec![(adapter.key.clone(), adapter.display_name.clone())],
            ));
        }
    }
    grouped
        .into_iter()
        .filter_map(|(relative_skills_dir, agents)| {
            let (key, first_display_name) = agents.first()?.clone();
            let display_name = if agents.len() == 1 {
                first_display_name
            } else {
                agents
                    .into_iter()
                    .map(|(_, display_name)| display_name)
                    .collect::<Vec<_>>()
                    .join(" / ")
            };
            Some(AgentSkillConfig {
                key,
                display_name,
                relative_skills_dir,
            })
        })
        .collect()
}

fn db_error(_: anyhow::Error) -> AiCommandError {
    command_error(
        AiErrorKind::Storage,
        AiErrorCode::Db,
        "Unable to query the skill store for AI analysis.",
        true,
    )
}

fn not_found(subject: &str) -> AiCommandError {
    command_error(
        AiErrorKind::State,
        AiErrorCode::NotFound,
        format!("{subject} no longer exists."),
        false,
    )
}

fn invalid_target(reason: &str) -> AiCommandError {
    command_error(
        AiErrorKind::Validation,
        AiErrorCode::InvalidTarget,
        format!("Invalid AI analysis target: {reason}."),
        false,
    )
}

fn ambiguous_target() -> AiCommandError {
    command_error(
        AiErrorKind::Validation,
        AiErrorCode::AmbiguousTarget,
        "The skill exists in both enabled and disabled roots; choose one directory.",
        false,
    )
}

fn unsafe_path() -> AiCommandError {
    command_error(
        AiErrorKind::Security,
        AiErrorCode::UnsafePath,
        "The skill document path is not inside an allowed root.",
        false,
    )
}

fn unreadable() -> AiCommandError {
    command_error(
        AiErrorKind::Provider,
        AiErrorCode::UnreadableDocument,
        "The skill document is not readable.",
        false,
    )
}

fn unreadable_outcome() -> (DocumentOutcome, Option<CollectedDocument>) {
    (
        DocumentOutcome::Unreadable {
            error_code: AiErrorCode::UnreadableDocument,
        },
        None,
    )
}

fn unsafe_outcome() -> (DocumentOutcome, Option<CollectedDocument>) {
    (
        DocumentOutcome::Unreadable {
            error_code: AiErrorCode::UnsafePath,
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ai::types::AiTargetRef;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_document(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut file = std::fs::File::create(dir.join(name)).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn outcome_of(
        store: &SkillStore,
        target: &AiTargetRef,
    ) -> (DocumentOutcome, Option<CollectedDocument>) {
        collect_document(store, target)
    }

    #[test]
    fn relative_path_rejects_escape_and_alias_components() {
        for path in [
            "../escape",
            "a/../../escape",
            "/absolute",
            "a\\b",
            "a//b",
            "a/./b",
            "a/.hidden/..",
            "a/\0b",
            "",
            "  ",
        ] {
            assert!(
                validate_relative_path(path).is_err(),
                "path should be rejected: {path}"
            );
        }
        assert!(validate_relative_path("nested/Skill-1").is_ok());
    }

    #[test]
    fn unknown_targets_return_structured_unreadable() {
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("doc.db")).unwrap();

        let (outcome, document) = outcome_of(
            &store,
            &AiTargetRef::Managed {
                skill_id: "missing".into(),
            },
        );
        assert_eq!(
            outcome,
            DocumentOutcome::Unreadable {
                error_code: AiErrorCode::NotFound
            }
        );
        assert!(document.is_none());
    }

    #[test]
    fn managed_skill_reads_first_level_candidate_in_priority_order() {
        let _guard = super::CENTRAL_ROOT_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("managed.db")).unwrap();

        // Insert a managed skill whose central_path points at a temp directory
        // that mimics the center skills root layout for the test.
        let skill_root = directory.path().join("skills");
        let skill_dir = skill_root.join("demo");
        write_document(&skill_dir, "SKILL.md", "# Demo\n\nCenter content");
        write_document(&skill_dir, "README.md", "README content");
        crate::core::central_repo::set_runtime_skills_dir_override(Some(skill_root.clone()));
        store
            .insert_skill(&crate::core::skill_store::SkillRecord {
                id: "managed-1".into(),
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

        let (outcome, document) = outcome_of(
            &store,
            &AiTargetRef::Managed {
                skill_id: "managed-1".into(),
            },
        );
        assert_eq!(outcome, DocumentOutcome::Ready);
        let document = document.unwrap();
        assert_eq!(document.document_filename, "SKILL.md");
        assert!(document.content.contains("Center content"));
        assert_eq!(document.source_hash.len(), 64);
        assert!(document.character_count > 0);

        crate::core::central_repo::set_runtime_skills_dir_override(None);
    }

    #[test]
    fn oversized_document_is_aborted_not_loaded() {
        let _guard = super::CENTRAL_ROOT_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let store = SkillStore::new(&directory.path().join("oversize.db")).unwrap();
        let skill_root = directory.path().join("skills");
        let skill_dir = skill_root.join("big");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut file = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        let chunk = vec![b'x'; 16_384];
        while file.metadata().unwrap().len() <= MAX_DOCUMENT_BYTES as u64 {
            file.write_all(&chunk).unwrap();
        }
        drop(file);

        crate::core::central_repo::set_runtime_skills_dir_override(Some(skill_root.clone()));
        store
            .insert_skill(&crate::core::skill_store::SkillRecord {
                id: "managed-big".into(),
                name: "Big".into(),
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

        let (outcome, document) = outcome_of(
            &store,
            &AiTargetRef::Managed {
                skill_id: "managed-big".into(),
            },
        );
        assert_eq!(
            outcome,
            DocumentOutcome::Unreadable {
                error_code: AiErrorCode::DocumentTooLarge
            }
        );
        assert!(document.is_none());
        crate::core::central_repo::set_runtime_skills_dir_override(None);
    }

    #[test]
    fn symlink_document_candidate_is_skipped() {
        let _guard = super::CENTRAL_ROOT_LOCK.lock().unwrap();
        #[cfg(unix)]
        {
            let directory = tempdir().unwrap();
            let store = SkillStore::new(&directory.path().join("symlink.db")).unwrap();
            let skill_root = directory.path().join("skills");
            let skill_dir = skill_root.join("demo");
            std::fs::create_dir_all(&skill_dir).unwrap();
            let secret = directory.path().join("secret.md");
            std::fs::write(&secret, "SECRET CONTENT").unwrap();
            std::os::unix::fs::symlink(&secret, skill_dir.join("SKILL.md")).unwrap();
            write_document(&skill_dir, "README.md", "Safe README");

            crate::core::central_repo::set_runtime_skills_dir_override(Some(skill_root.clone()));
            store
                .insert_skill(&crate::core::skill_store::SkillRecord {
                    id: "managed-symlink".into(),
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

            let (outcome, document) = outcome_of(
                &store,
                &AiTargetRef::Managed {
                    skill_id: "managed-symlink".into(),
                },
            );
            assert_eq!(outcome, DocumentOutcome::Ready);
            let document = document.unwrap();
            assert_eq!(document.document_filename, "README.md");
            assert!(!document.content.contains("SECRET CONTENT"));
            crate::core::central_repo::set_runtime_skills_dir_override(None);
        }
    }
}
