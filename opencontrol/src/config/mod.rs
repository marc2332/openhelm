use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub ai: AiConfig,
    pub telegram: TelegramConfig,
    pub audit: AuditConfig,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    pub socket_path: String,
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AiConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub system_prompt: String,
    /// Inactivity timeout in minutes before session auto-resets
    pub session_timeout_minutes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    #[serde(default)]
    pub users: Vec<TelegramUser>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramUser {
    pub telegram_id: i64,
    pub name: String,
    /// Name of the profile assigned to this user (must exist in [profiles])
    pub profile: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Profile {
    /// Optional system prompt override; falls back to [ai].system_prompt
    pub system_prompt: Option<String>,
    /// Optional model override; falls back to [ai].model
    pub model: Option<String>,
    #[serde(default)]
    pub permissions: ProfilePermissions,
    /// Filesystem path allowlists; required when permissions.fs = true
    pub fs: Option<FsPermissions>,
    /// Telegram file attachment settings
    pub attachments: Option<AttachmentsConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProfilePermissions {
    /// Enable the filesystem tool group
    #[serde(default)]
    pub fs: bool,
    /// Skill configs keyed by skill name (e.g. "github").
    /// Presence of a key enables the skill; the value is passed to the skill's
    /// build_tools() as its per-profile config table.
    #[serde(default)]
    pub skills: HashMap<String, toml::Value>,
}

/// Per-operation filesystem path allowlists.
/// An empty list for an operation means that operation is completely disabled.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FsPermissions {
    /// Paths the AI may read files from
    #[serde(default)]
    pub read: Vec<String>,
    /// Paths the AI may list directory entries under
    #[serde(default)]
    pub read_dir: Vec<String>,
    /// Paths the AI may write or overwrite files in
    #[serde(default)]
    pub write: Vec<String>,
    /// Paths the AI may create directories under
    #[serde(default)]
    pub mkdir: Vec<String>,
}

impl FsPermissions {
    /// Returns true if at least one operation has at least one allowed path.
    #[allow(dead_code)]
    pub fn has_any_paths(&self) -> bool {
        !self.read.is_empty()
            || !self.read_dir.is_empty()
            || !self.write.is_empty()
            || !self.mkdir.is_empty()
    }
}

/// Configuration for Telegram file attachment handling.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttachmentsConfig {
    /// Master switch: whether the bot processes file attachments at all.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed file extensions (without leading dot, case-insensitive).
    /// e.g. ["txt", "csv", "jpg", "png"]
    /// Image extensions (jpg, jpeg, png, gif, webp) are sent as images to the AI.
    /// Text extensions (txt, csv, json, log, md, etc.) are read as UTF-8 text.
    #[serde(default)]
    pub allowed_extensions: Vec<String>,
    /// Maximum file size in bytes. Defaults to 5 MiB.
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: u64,
}

const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

fn default_max_file_size() -> u64 {
    DEFAULT_MAX_FILE_SIZE_BYTES
}

/// Well-known image extensions that are sent to the AI as image content.
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuditConfig {
    pub log_path: String,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        Self {
            daemon: DaemonConfig {
                socket_path: "/tmp/opencontrol.sock".to_string(),
                log_level: "info".to_string(),
            },
            ai: AiConfig {
                api_url: "https://openrouter.ai/api/v1".to_string(),
                api_key: String::new(),
                model: "gpt-4o".to_string(),
                system_prompt: "You are a helpful assistant with access to tools on the host system. Use them carefully and only when necessary.".to_string(),
                session_timeout_minutes: 30,
            },
            telegram: TelegramConfig {
                bot_token: String::new(),
                users: vec![],
            },
            audit: AuditConfig {
                log_path: format!("{}/.local/share/opencontrol/audit.log", home),
            },
            profiles: HashMap::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join("opencontrol.toml")
    }

    pub async fn load() -> Result<Self> {
        let path = Self::path();
        Self::load_from(&path).await
    }

    pub async fn load_from(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read config from {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config from {}", path.display()))?;
        Ok(config)
    }

    pub async fn save(&self) -> Result<()> {
        let path = Self::path();
        self.save_to(&path).await
    }

    pub async fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let contents = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(path, contents)
            .await
            .with_context(|| format!("Failed to write config to {}", path.display()))?;
        Ok(())
    }

    /// Find a user by telegram_id.
    pub fn find_user(&self, telegram_id: i64) -> Option<&TelegramUser> {
        self.telegram
            .users
            .iter()
            .find(|user| user.telegram_id == telegram_id)
    }

    /// Resolve a profile by name, returning a clear error if it doesn't exist.
    pub fn resolve_profile<'a>(&'a self, name: &str) -> Result<&'a Profile> {
        self.profiles.get(name).ok_or_else(|| {
            let defined = if self.profiles.is_empty() {
                "(none defined)".to_string()
            } else {
                self.profiles
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            anyhow::anyhow!(
                "Profile '{}' not found. Defined profiles: {}",
                name,
                defined
            )
        })
    }

    /// Validate that a profile name exists. Hard error if not.
    pub fn require_profile(&self, name: &str) -> Result<()> {
        self.resolve_profile(name).map(|_| ())
    }

    /// Resolve the effective model for a user (profile → global fallback).
    pub fn effective_model(&self, user: &TelegramUser) -> String {
        self.profiles
            .get(&user.profile)
            .and_then(|profile| profile.model.clone())
            .unwrap_or_else(|| self.ai.model.clone())
    }

    /// Resolve the effective system prompt for a user (profile → global fallback).
    pub fn effective_system_prompt(&self, user: &TelegramUser) -> String {
        self.profiles
            .get(&user.profile)
            .and_then(|profile| profile.system_prompt.clone())
            .unwrap_or_else(|| self.ai.system_prompt.clone())
    }
}
