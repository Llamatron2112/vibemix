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
