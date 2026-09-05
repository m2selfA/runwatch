use anyhow::{Context, Result};
use runwatch_core::data_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use windui::prelude::{Signal, signal};

pub const GUI_SETTINGS_VERSION: u32 = 1;
/// WindUI 0.15.0 exposes runtime tooltip mutation but not runtime native
/// notifications yet. Keep the product surface fail-closed until the public
/// TrayHandle notification bridge is released upstream (wind-ui-rust #13).
pub const NATIVE_NOTIFICATION_AVAILABLE: bool = false;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GuiSettings {
    pub version: u32,
    pub native_notifications: bool,
    pub notify_success: bool,
    pub notify_attention: bool,
    pub include_run_name: bool,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            version: GUI_SETTINGS_VERSION,
            native_notifications: NATIVE_NOTIFICATION_AVAILABLE,
            notify_success: true,
            notify_attention: true,
            include_run_name: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedGuiSettings {
    pub settings: GuiSettings,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct GuiSettingsState {
    pub native_notifications: Signal<bool>,
    pub notify_success: Signal<bool>,
    pub notify_attention: Signal<bool>,
    pub include_run_name: Signal<bool>,
    pub warning: Signal<String>,
}

impl GuiSettingsState {
    pub fn new(loaded: LoadedGuiSettings) -> Self {
        Self {
            native_notifications: signal(loaded.settings.native_notifications),
            notify_success: signal(loaded.settings.notify_success),
            notify_attention: signal(loaded.settings.notify_attention),
            include_run_name: signal(loaded.settings.include_run_name),
            warning: signal(loaded.warning.unwrap_or_default()),
        }
    }

    pub fn snapshot(&self) -> GuiSettings {
        GuiSettings {
            version: GUI_SETTINGS_VERSION,
            native_notifications: self.native_notifications.get(),
            notify_success: self.notify_success.get(),
            notify_attention: self.notify_attention.get(),
            include_run_name: self.include_run_name.get(),
        }
    }
}

pub fn load() -> LoadedGuiSettings {
    match settings_path() {
        Ok(path) => load_at(&path),
        Err(error) => LoadedGuiSettings {
            settings: GuiSettings::default(),
            warning: Some(format!("Could not resolve GUI settings path: {error:#}")),
        },
    }
}

pub fn save(settings: GuiSettings) -> Result<()> {
    let path = settings_path()?;
    save_at(&path, settings)
}

fn settings_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("gui-settings.json"))
}

fn load_at(path: &Path) -> LoadedGuiSettings {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedGuiSettings {
                settings: GuiSettings::default(),
                warning: None,
            };
        }
        Err(error) => {
            return LoadedGuiSettings {
                settings: GuiSettings::default(),
                warning: Some(format!(
                    "Could not read {}: {error}; using defaults",
                    path.display()
                )),
            };
        }
    };
    match serde_json::from_str::<GuiSettings>(&text) {
        Ok(settings) if settings.version == GUI_SETTINGS_VERSION => LoadedGuiSettings {
            settings,
            warning: None,
        },
        Ok(settings) => LoadedGuiSettings {
            settings: GuiSettings::default(),
            warning: Some(format!(
                "Unsupported GUI settings version {} in {}; using defaults",
                settings.version,
                path.display()
            )),
        },
        Err(error) => LoadedGuiSettings {
            settings: GuiSettings::default(),
            warning: Some(format!(
                "Could not parse {}: {error}; using defaults",
                path.display()
            )),
        },
    }
}

fn save_at(path: &Path, mut settings: GuiSettings) -> Result<()> {
    settings.version = GUI_SETTINGS_VERSION;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create GUI settings directory {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(&settings).context("serialize GUI settings")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("write GUI settings {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "runwatch-gui-settings-{}-{}-{name}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn missing_settings_use_defaults_without_warning() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        let loaded = load_at(&path);
        assert_eq!(loaded.settings, GuiSettings::default());
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn corrupt_or_future_settings_fail_soft_to_defaults() {
        let corrupt = temp_path("corrupt");
        fs::write(&corrupt, b"{not-json").unwrap();
        let loaded = load_at(&corrupt);
        assert_eq!(loaded.settings, GuiSettings::default());
        assert!(
            loaded
                .warning
                .as_deref()
                .unwrap_or_default()
                .contains("using defaults")
        );
        let _ = fs::remove_file(&corrupt);

        let future = temp_path("future");
        fs::write(
            &future,
            br#"{"version":99,"native_notifications":false,"notify_success":false,"notify_attention":false,"include_run_name":false}"#,
        )
        .unwrap();
        let loaded = load_at(&future);
        assert_eq!(loaded.settings, GuiSettings::default());
        assert!(
            loaded
                .warning
                .as_deref()
                .unwrap_or_default()
                .contains("Unsupported")
        );
        let _ = fs::remove_file(&future);
    }

    #[test]
    fn settings_roundtrip_preserves_only_desktop_preferences() {
        let path = temp_path("roundtrip");
        let settings = GuiSettings {
            version: 7,
            native_notifications: false,
            notify_success: false,
            notify_attention: true,
            include_run_name: false,
        };
        save_at(&path, settings).unwrap();
        let loaded = load_at(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.settings.version, GUI_SETTINGS_VERSION);
        assert!(!loaded.settings.native_notifications);
        assert!(!loaded.settings.notify_success);
        assert!(loaded.settings.notify_attention);
        assert!(!loaded.settings.include_run_name);
        let _ = fs::remove_file(&path);
    }
}
