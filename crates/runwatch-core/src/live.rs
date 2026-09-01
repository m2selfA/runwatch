use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{data_dir, ensure_data_dir};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeHeartbeat {
    pub pid: u32,
    pub beat_unix: u64,
}

pub fn heartbeat_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("serve.pid"))
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("serve.lock"))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn read() -> Result<Option<ServeHeartbeat>> {
    let path = heartbeat_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text).ok())
}

pub fn is_alive() -> bool {
    match read() {
        Ok(Some(hb)) => now_unix().saturating_sub(hb.beat_unix) < 45,
        _ => false,
    }
}

pub fn owner_lock_held() -> Result<bool> {
    ensure_data_dir()?;
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open serve lock {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

pub fn write_beat() -> Result<()> {
    ensure_data_dir()?;
    let hb = ServeHeartbeat {
        pid: std::process::id(),
        beat_unix: now_unix(),
    };
    fs::write(heartbeat_path()?, serde_json::to_string(&hb)?)?;
    Ok(())
}

pub fn clear_if_ours() {
    if let Ok(Some(hb)) = read() {
        if hb.pid == std::process::id() {
            let _ = fs::remove_file(heartbeat_path().unwrap_or_default());
        }
    }
}

pub struct ServeLock {
    file: fs::File,
}

impl ServeLock {
    pub fn claim() -> Result<Self> {
        ensure_data_dir()?;
        let path = lock_path()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open serve lock {}", path.display()))?;
        if let Err(err) = file.try_lock_exclusive() {
            bail!("runwatch serve already owns {}: {err}", path.display());
        }
        write_beat()?;
        Ok(Self { file })
    }
}

impl Drop for ServeLock {
    fn drop(&mut self) {
        clear_if_ours();
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_file_lock_rejects_second_owner() {
        let path = std::env::temp_dir().join(format!(
            "runwatch-live-lock-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let first = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("first lock file");
        first.try_lock_exclusive().expect("first lock");
        let second = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("second lock file");
        assert!(second.try_lock_exclusive().is_err());
        first.unlock().expect("unlock first");
        second
            .try_lock_exclusive()
            .expect("second can acquire after release");
        second.unlock().expect("unlock second");
        let _ = fs::remove_file(path);
    }
}
