use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ClaudeMode {
    /// The `claude` command already signed in on this computer
    Cli,
    /// An Anthropic API key
    Api,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Run the safe cleanup rules on their own every few hours
    pub auto_cleanup: bool,
    pub auto_cleanup_hours: u64,
    /// Trash older than this many days is emptied by the safe rule
    pub trash_days: u64,
    /// Program names that get lower CPU and disk priority whenever the disk is choking
    pub calm_apps: Vec<String>,
    pub auto_calm: bool,
    pub claude_mode: ClaudeMode,
    pub api_key: String,
    pub model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_cleanup: true,
            auto_cleanup_hours: 6,
            trash_days: 30,
            calm_apps: vec![],
            auto_calm: false,
            claude_mode: ClaudeMode::Cli,
            api_key: String::new(),
            model: "claude-opus-5-5".into(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("membli")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("membli")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(config_dir().join("config.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");
        {
            use std::io::Write;
            // 0600: the file can hold an API key
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}
