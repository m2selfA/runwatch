use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::types::RunnerKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub ssh: SshSettings,
    #[serde(default)]
    pub hosts: Vec<HostWatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshSettings {
    #[serde(default = "default_alive")]
    pub alive_interval: u64,
    #[serde(default = "default_timeout")]
    pub cmd_timeout_sec: u64,
}

fn default_alive() -> u64 {
    30
}
fn default_timeout() -> u64 {
    20
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            alive_interval: default_alive(),
            cmd_timeout_sec: default_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostWatch {
    pub alias: String,
    #[serde(default = "default_poll")]
    pub poll_sec: u64,
    #[serde(default)]
    pub query: RunnerKind,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_poll() -> u64 {
    20
}
fn default_enabled() -> bool {
    true
}

impl Default for RunnerKind {
    fn default() -> Self {
        Self::Slurm
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ssh: SshSettings::default(),
            hosts: Vec::new(),
        }
    }
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RUNWATCH_DATA_DIR") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .context("cannot resolve home directory")?;
    Ok(home.join(".runwatch"))
}

pub fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict runwatch data directory {}", dir.display()))?;
    }
    Ok(dir)
}

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        Ok(data_dir()?.join("config.yaml"))
    }

    pub fn load_or_default() -> Result<Self> {
        let path = Self::path()?;
        if path.exists() {
            Self::load(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Ok(serde_yaml::from_str(&text)?)
    }

    pub fn save(&self) -> Result<()> {
        let dir = ensure_data_dir()?;
        let path = dir.join("config.yaml");
        fs::write(&path, serde_yaml::to_string(self)?)?;
        Ok(())
    }
}
