use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::permissions::Permission;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub ai: AiConfig,
    pub telegram: TelegramConfig,
    pub audit: AuditConfig,
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
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub fs_allowed_paths: Vec<String>,
    /// Optional per-user model override
    pub model: Option<String>,
}

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
                api_url: "https://api.openai.com/v1".to_string(),
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
        fs::write(path, contents).await.with_context(|| {
            format!("Failed to write config to {}", path.display())
        })?;
        Ok(())
    }

    /// Find a user by telegram_id
    pub fn find_user(&self, telegram_id: i64) -> Option<&TelegramUser> {
        self.telegram.users.iter().find(|u| u.telegram_id == telegram_id)
    }

    /// Find a user mutably by telegram_id
    #[allow(dead_code)]
    pub fn find_user_mut(&mut self, telegram_id: i64) -> Option<&mut TelegramUser> {
        self.telegram.users.iter_mut().find(|u| u.telegram_id == telegram_id)
    }
}
