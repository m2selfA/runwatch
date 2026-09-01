use anyhow::{Context, Result, bail};
use directories::UserDirs;
use serde::Deserialize;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const MAX_SESSION_FILES: usize = 20_000;
const MAX_SESSION_META_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub thread_id: String,
    pub cwd: String,
    pub session_file: String,
}

#[derive(Debug, Deserialize)]
struct SessionMetaRecord {
    #[serde(rename = "type")]
    kind: String,
    payload: SessionMetaPayload,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    #[serde(default)]
    session_id: Option<String>,
    cwd: String,
}

fn validate_thread_id(thread_id: &str) -> Result<()> {
    if thread_id.is_empty()
        || thread_id.len() > 128
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("Codex thread id is not a bounded ASCII identifier");
    }
    Ok(())
}

fn codex_home() -> Result<PathBuf> {
    if let Some(value) = env::var_os("CODEX_HOME")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let home = UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .context("cannot resolve home directory for CODEX_HOME")?;
    Ok(home.join(".codex"))
}

fn read_session_meta(path: &Path, expected_thread_id: &str) -> Result<Option<CodexSessionMeta>> {
    let file =
        File::open(path).with_context(|| format!("open Codex rollout {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut limited = reader.take(MAX_SESSION_META_BYTES + 1);
    let mut first_line = String::new();
    limited
        .read_line(&mut first_line)
        .with_context(|| format!("read Codex session_meta from {}", path.display()))?;
    if first_line.len() as u64 > MAX_SESSION_META_BYTES {
        bail!(
            "Codex session_meta exceeds {} bytes in {}",
            MAX_SESSION_META_BYTES,
            path.display()
        );
    }
    let record: SessionMetaRecord = serde_json::from_str(first_line.trim_end())
        .with_context(|| format!("parse Codex session_meta from {}", path.display()))?;
    if record.kind != "session_meta" {
        bail!(
            "Codex rollout {} does not start with session_meta",
            path.display()
        );
    }
    if record.payload.id != expected_thread_id {
        return Ok(None);
    }
    if let Some(session_id) = record.payload.session_id.as_deref()
        && session_id != record.payload.id
    {
        bail!(
            "Codex session_meta identity mismatch: id={} session_id={session_id}",
            record.payload.id
        );
    }
    if record.payload.cwd.trim().is_empty() {
        bail!("Codex session {expected_thread_id} has an empty cwd");
    }
    let cwd = PathBuf::from(&record.payload.cwd);
    if !cwd.is_dir() {
        bail!(
            "Codex session {expected_thread_id} cwd is unavailable: {}",
            cwd.display()
        );
    }
    Ok(Some(CodexSessionMeta {
        thread_id: record.payload.id,
        cwd: record.payload.cwd,
        session_file: path.to_string_lossy().into_owned(),
    }))
}

pub fn locate_codex_session(thread_id: &str) -> Result<CodexSessionMeta> {
    let root = codex_home()?.join("sessions");
    locate_codex_session_in(&root, thread_id)
}

pub(crate) fn locate_codex_session_in(root: &Path, thread_id: &str) -> Result<CodexSessionMeta> {
    validate_thread_id(thread_id)?;
    if !root.is_dir() {
        bail!(
            "Codex sessions directory is unavailable: {}",
            root.display()
        );
    }

    let mut stack = vec![root.to_path_buf()];
    let mut inspected = 0usize;
    let mut match_found: Option<CodexSessionMeta> = None;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            inspected += 1;
            if inspected > MAX_SESSION_FILES {
                bail!(
                    "Codex session lookup exceeded {MAX_SESSION_FILES} files under {}",
                    root.display()
                );
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".jsonl") || !name.contains(thread_id) {
                continue;
            }
            if let Some(meta) = read_session_meta(&path, thread_id)? {
                if match_found.is_some() {
                    bail!("multiple Codex rollout files match thread {thread_id}");
                }
                match_found = Some(meta);
            }
        }
    }

    match_found.with_context(|| format!("no persisted Codex rollout found for thread {thread_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "runwatch-codex-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(root.join("2026/09/01")).unwrap();
        root
    }

    #[test]
    fn locator_reads_only_first_session_meta_record_and_returns_exact_cwd() {
        let root = temp_root("session-meta");
        let thread = "019c1234-5678-7000-8000-000000000001";
        let cwd = root.join("science");
        fs::create_dir_all(&cwd).unwrap();
        let rollout = root
            .join("2026/09/01")
            .join(format!("rollout-2026-09-01T00-00-00-{thread}.jsonl"));
        let first = serde_json::json!({
            "timestamp": "2026-09-01T00:00:00Z",
            "type": "session_meta",
            "payload": { "id": thread, "cwd": cwd.to_string_lossy() }
        });
        fs::write(
            &rollout,
            format!("{}\nTHIS SECOND RECORD IS DELIBERATELY NOT JSON\n", first),
        )
        .unwrap();

        let meta = locate_codex_session_in(&root, thread).unwrap();
        assert_eq!(meta.thread_id, thread);
        assert_eq!(PathBuf::from(meta.cwd), cwd);
        assert_eq!(PathBuf::from(meta.session_file), rollout);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires RUNWATCH_R9_CODEX_THREAD pointing at a real persisted Codex rollout"]
    fn real_codex_session_locator_acceptance() {
        let thread = env::var("RUNWATCH_R9_CODEX_THREAD")
            .expect("RUNWATCH_R9_CODEX_THREAD is required for ignored R9 acceptance");
        let meta = locate_codex_session(&thread).expect("locate real Codex rollout");
        assert_eq!(meta.thread_id, thread);
        assert!(PathBuf::from(&meta.cwd).is_dir());
        assert!(PathBuf::from(&meta.session_file).is_file());
    }

    #[test]
    fn locator_fails_closed_when_metadata_id_or_cwd_cannot_be_bound() {
        let root = temp_root("fail-closed");
        let thread = "019c1234-5678-7000-8000-000000000002";
        let rollout = root
            .join("2026/09/01")
            .join(format!("rollout-2026-09-01T00-00-00-{thread}.jsonl"));
        fs::write(
            &rollout,
            serde_json::json!({
                "type": "session_meta",
                "payload": { "id": "different-thread", "cwd": root.to_string_lossy() }
            })
            .to_string(),
        )
        .unwrap();
        assert!(locate_codex_session_in(&root, thread).is_err());

        fs::write(
            &rollout,
            serde_json::json!({
                "type": "session_meta",
                "payload": { "id": thread, "cwd": root.join("missing").to_string_lossy() }
            })
            .to_string(),
        )
        .unwrap();
        let error = locate_codex_session_in(&root, thread).unwrap_err();
        assert!(error.to_string().contains("cwd is unavailable"));
        let _ = fs::remove_dir_all(root);
    }
}
