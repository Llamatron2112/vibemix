use std::path::PathBuf;

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
