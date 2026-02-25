use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::config::FsPermissions;

// ─── Request / Response types ─────────────────────────────────────────────────

/// Commands sent from CLI -> daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    Status,
    PairList,
    PairApprove {
        telegram_id: i64,
        /// Profile name — must exist in config or daemon returns an error
        profile: String,
    },
    PairReject {
        telegram_id: i64,
    },
    UsersList,
    UserRemove {
        telegram_id: i64,
    },
    ProfilesList,
    /// Send a message as a CLI-initiated AI session
    Chat {
        message: String,
        /// Profile to use for this CLI session (must exist in config)
        profile: String,
    },
    ChatReset {
        /// Profile whose CLI session should be reset
        profile: String,
    },
    /// Fetch recent in-memory daemon log lines.
    /// Use offset=0 with lines>0 to get the last N lines.
    /// Use a previously returned `total` as `offset` to poll for new lines only.
    Logs {
        /// Max lines to return (used when offset=0)
        lines: usize,
        /// Return only lines after this absolute offset (for follow mode)
        offset: usize,
    },
    Shutdown,
}

/// Responses sent from daemon -> CLI.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IpcResponse {
    Ok {
        message: String,
    },
    Error {
        message: String,
    },
    Status {
        uptime_seconds: u64,
        active_sessions: usize,
        paired_users: usize,
        pending_pairs: usize,
        telegram_connected: bool,
    },
    PairList {
        pending: Vec<PendingPair>,
    },
    UsersList {
        users: Vec<UserInfo>,
    },
    ProfilesList {
        profiles: Vec<ProfileInfo>,
    },
    ChatReply {
        message: String,
    },
    Logs {
        lines: Vec<String>,
        /// New total count — pass back as `offset` on next poll
        total: usize,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PendingPair {
    pub telegram_id: i64,
    pub username: String,
    pub requested_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserInfo {
    pub telegram_id: i64,
    pub name: String,
    pub profile: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub model: Option<String>,
    pub has_custom_prompt: bool,
    pub fs_enabled: bool,
    pub fs: Option<FsPermissions>,
}

// ─── Wire protocol ────────────────────────────────────────────────────────────

pub async fn send_request(stream: &mut UnixStream, req: &IpcRequest) -> Result<()> {
    let mut line = serde_json::to_string(req).context("Failed to serialize IPC request")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .context("Failed to write IPC request")?;
    Ok(())
}

pub async fn recv_response(stream: &mut UnixStream) -> Result<IpcResponse> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("Failed to read IPC response")?;
    serde_json::from_str(line.trim()).context("Failed to parse IPC response")
}

pub async fn send_response(stream: &mut UnixStream, resp: &IpcResponse) -> Result<()> {
    let mut line = serde_json::to_string(resp).context("Failed to serialize IPC response")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .context("Failed to write IPC response")?;
    Ok(())
}

pub async fn recv_request(stream: &mut UnixStream) -> Result<IpcRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("Failed to read IPC request")?;
    serde_json::from_str(line.trim()).context("Failed to parse IPC request")
}

pub async fn client_call(socket_path: &str, req: &IpcRequest) -> Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!("Cannot connect to daemon socket at {socket_path}. Is the daemon running?")
    })?;
    send_request(&mut stream, req).await?;
    recv_response(&mut stream).await
}
