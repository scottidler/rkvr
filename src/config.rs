use eyre::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// XDG config dir, honoring `$XDG_CONFIG_HOME` and falling back to `$HOME/.config`.
pub fn xdg_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

/// XDG data dir, honoring `$XDG_DATA_HOME` and falling back to `$HOME/.local/share`.
///
/// We deliberately do NOT use the `dirs` config/data helpers: those honor
/// `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` only on Linux. On macOS they resolve via system
/// APIs and return `~/Library/...`, ignoring the env vars. These helpers resolve to the
/// same XDG layout on every platform.
pub fn xdg_data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::home_dir().map(|h| h.join(".local").join("share"))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_cleanup_days")]
    pub cleanup_days: usize,

    #[serde(default)]
    pub auto_cleanup: bool,

    #[serde(default = "default_archive_location")]
    pub archive_location: String,
}

fn default_cleanup_days() -> usize {
    30
}

fn default_archive_location() -> String {
    xdg_data_dir()
        .map(|d| d.join("rkvr").join("archive"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.local/share/rkvr/archive".to_string())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cleanup_days: default_cleanup_days(),
            auto_cleanup: false,
            archive_location: default_archive_location(),
        }
    }
}

impl Config {
    pub fn load(config_path: Option<PathBuf>) -> Result<Self> {
        let config_file = match config_path {
            Some(path) => path,
            None => Self::find_config_file()?,
        };

        if config_file.exists() {
            let contents = fs::read_to_string(&config_file)?;
            let config: Config = serde_yaml::from_str(&contents)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    fn find_config_file() -> Result<PathBuf> {
        let candidates = vec![
            xdg_config_dir().map(|d| d.join("rkvr").join("rkvr.yml")),
            Some(PathBuf::from("./rkvr.yml")),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        // Return primary location even if it doesn't exist
        Ok(xdg_config_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine config directory"))?
            .join("rkvr")
            .join("rkvr.yml"))
    }
}
