use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Global launch preferences, stored next to the other configuration files.
///
/// This is deliberately not part of a session: it controls what happens before
/// a session is loaded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StartupSettings {
    /// Open the last saved/opened session when the engine starts.
    #[serde(default)]
    pub load_last_session: bool,
    /// Last session path opened or saved by the GUI.
    #[serde(default)]
    pub last_session: String,
}

pub fn startup_settings_path() -> PathBuf {
    config_dir().join("startup.json")
}

/// Read the launch preferences, falling back to defaults when the file is
/// missing or unreadable.
pub fn load_startup_settings() -> StartupSettings {
    std::fs::read_to_string(startup_settings_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_startup_settings(settings: &StartupSettings) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    write_atomic(&startup_settings_path(), text)
}

/// The vibemix configuration directory, shared by the engine and the GUI.
///
/// `VIBEMIX_CONFIG_DIR` overrides `$XDG_CONFIG_HOME/vibemix`, which falls back
/// to `~/.config/vibemix` and finally the current directory.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("VIBEMIX_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("vibemix");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/vibemix");
    }
    PathBuf::from(".")
}

/// Writes `contents` to `path` without ever leaving a truncated file behind:
/// the data goes to a sibling temp file, is flushed to disk, and then replaces
/// the destination with an atomic rename.
pub fn write_atomic(path: &std::path::Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_settings_roundtrip() {
        let settings = StartupSettings {
            load_last_session: true,
            last_session: "/tmp/mix.toml".to_string(),
        };
        let text = serde_json::to_string_pretty(&settings).unwrap();
        let parsed: StartupSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, settings);

        let defaults: StartupSettings = serde_json::from_str("{}").unwrap();
        assert!(!defaults.load_last_session);
        assert!(defaults.last_session.is_empty());
    }

    #[test]
    fn write_atomic_replaces_contents_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("vibemix-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.toml");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
