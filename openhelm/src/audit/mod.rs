use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::path::PathBuf;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::mpsc,
};
use tracing::warn;

/// All audit event types.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum AuditEvent {
    SessionStart {
        user_id: i64,
        username: String,
        channel: Channel,
        session_id: String,
    },
    SessionEnd {
        user_id: i64,
        username: String,
        session_id: String,
    },
    MessageSent {
        user_id: i64,
        session_id: String,
        preview: String,
        model: String,
    },
    ToolCall {
        user_id: i64,
        session_id: String,
        tool: String,
        args: serde_json::Value,
        allowed: bool,
    },
    ToolResult {
        user_id: i64,
        session_id: String,
        tool: String,
        success: bool,
        error: Option<String>,
    },
    PairingRequest {
        telegram_id: i64,
        username: String,
    },
    PairingDecision {
        telegram_id: i64,
        approved: bool,
        decided_by: String,
    },
    UserRemoved {
        telegram_id: i64,
        removed_by: String,
    },
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Telegram,
    Cli,
}

/// A serialized audit log entry with a timestamp.
#[derive(Serialize)]
struct LogEntry<'a> {
    ts: String,
    #[serde(flatten)]
    event: &'a AuditEvent,
}

/// Handle to the audit logger. Clone freely - backed by an async channel.
#[derive(Clone)]
pub struct AuditLogger {
    tx: mpsc::UnboundedSender<AuditEvent>,
}

impl AuditLogger {
    /// Spawn the background writer task and return a logger handle.
    pub async fn new(log_path: &str) -> Result<Self> {
        let path = PathBuf::from(log_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(writer_task(path, rx));

        Ok(Self { tx })
    }

    /// Fire-and-forget: log an audit event.
    pub fn log(&self, event: AuditEvent) {
        if self.tx.send(event).is_err() {
            warn!("Audit logger channel closed - event dropped");
        }
    }
}

/// Background task that serializes events and appends them to the log file.
async fn writer_task(path: PathBuf, mut rx: mpsc::UnboundedReceiver<AuditEvent>) {
    while let Some(event) = rx.recv().await {
        let entry = LogEntry {
            ts: Utc::now().to_rfc3339(),
            event: &event,
        };

        let mut line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to serialize audit event: {}", e);
                continue;
            }
        };
        line.push('\n');

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(line.as_bytes()).await {
                    warn!("Failed to write audit log: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to open audit log at {}: {}", path.display(), e);
            }
        }
    }
}
