use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::ai::client::AiClient;
use crate::ai::session::SessionManager;
use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{Config, TelegramUser};
use crate::ipc::{
    recv_request, send_response, IpcRequest, IpcResponse, PendingPair, UserInfo,
};
use crate::permissions::Permission;
use crate::telegram::{run_bot, BotState};
use crate::tools::ToolRegistry;

pub struct Daemon {
    config: Arc<RwLock<Config>>,
    sessions: Arc<SessionManager>,
    audit: AuditLogger,
    pending_pairs: Arc<RwLock<Vec<PendingPair>>>,
    bot_connected: Arc<AtomicBool>,
    start_time: Instant,
    socket_path: String,
}

impl Daemon {
    pub async fn new(config: Config) -> Result<Self> {
        let audit = AuditLogger::new(&config.audit.log_path).await?;
        let client = AiClient::new(&config.ai.api_url, &config.ai.api_key)?;
        let tools = Arc::new(ToolRegistry::new());
        let sessions = Arc::new(SessionManager::new(
            client,
            tools,
            audit.clone(),
            &config.ai.model,
            &config.ai.system_prompt,
            config.ai.session_timeout_minutes,
        ));
        let socket_path = config.daemon.socket_path.clone();

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            sessions,
            audit,
            pending_pairs: Arc::new(RwLock::new(vec![])),
            bot_connected: Arc::new(AtomicBool::new(false)),
            start_time: Instant::now(),
            socket_path,
        })
    }

    /// Run the daemon: IPC listener + Telegram bot concurrently.
    pub async fn run(self) -> Result<()> {
        // Remove stale socket
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("Failed to bind Unix socket at {}", self.socket_path))?;

        info!(socket = %self.socket_path, "Daemon listening on Unix socket");

        let bot_state = BotState {
            config: self.config.clone(),
            sessions: self.sessions.clone(),
            audit: self.audit.clone(),
            pending_pairs: self.pending_pairs.clone(),
            bot_connected: self.bot_connected.clone(),
            start_time: self.start_time,
        };

        let config_arc = self.config.clone();
        let sessions_arc = self.sessions.clone();
        let audit_arc = self.audit.clone();
        let pending_arc = self.pending_pairs.clone();
        let bot_connected_arc = self.bot_connected.clone();
        let start_time = self.start_time;

        // Spawn Telegram bot — runs independently; if it fails or has no token,
        // the daemon keeps running and serves IPC / CLI sessions.
        tokio::spawn(async move {
            if let Err(e) = run_bot(bot_state).await {
                error!(error = %e, "Telegram bot error");
            }
        });

        // IPC accept loop — runs until ctrl-c or Shutdown IPC command
        let ipc_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let config = config_arc.clone();
                        let sessions = sessions_arc.clone();
                        let audit = audit_arc.clone();
                        let pending = pending_arc.clone();
                        let bot_connected = bot_connected_arc.clone();
                        tokio::spawn(handle_ipc_connection(
                            stream, config, sessions, audit, pending, bot_connected, start_time,
                        ));
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to accept IPC connection");
                    }
                }
            }
        });

        tokio::select! {
            _ = ipc_handle => info!("IPC listener task ended"),
            _ = tokio::signal::ctrl_c() => info!("Received Ctrl+C, shutting down"),
        }

        Ok(())
    }
}

async fn handle_ipc_connection(
    mut stream: UnixStream,
    config: Arc<RwLock<Config>>,
    sessions: Arc<SessionManager>,
    audit: AuditLogger,
    pending: Arc<RwLock<Vec<PendingPair>>>,
    bot_connected: Arc<AtomicBool>,
    start_time: Instant,
) {
    let req = match recv_request(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to read IPC request");
            return;
        }
    };

    let resp = dispatch(req, config, sessions, audit, pending, bot_connected, start_time).await;

    if let Err(e) = send_response(&mut stream, &resp).await {
        warn!(error = %e, "Failed to send IPC response");
    }
}

async fn dispatch(
    req: IpcRequest,
    config: Arc<RwLock<Config>>,
    sessions: Arc<SessionManager>,
    audit: AuditLogger,
    pending: Arc<RwLock<Vec<PendingPair>>>,
    bot_connected: Arc<AtomicBool>,
    start_time: Instant,
) -> IpcResponse {
    match req {
        IpcRequest::Status => {
            let cfg = config.read().await;
            let pending_count = pending.read().await.len();
            IpcResponse::Status {
                uptime_seconds: start_time.elapsed().as_secs(),
                active_sessions: sessions.active_count().await,
                paired_users: cfg.telegram.users.len(),
                pending_pairs: pending_count,
                telegram_connected: bot_connected.load(std::sync::atomic::Ordering::Relaxed),
            }
        }

        IpcRequest::PairList => {
            let pairs = pending.read().await.clone();
            IpcResponse::PairList { pending: pairs }
        }

        IpcRequest::PairApprove {
            telegram_id,
            permissions,
            fs_allowed_paths,
        } => {
            let pair_info = {
                let pairs = pending.read().await;
                pairs.iter().find(|p| p.telegram_id == telegram_id).cloned()
            };

            let pair = match pair_info {
                Some(p) => p,
                None => {
                    return IpcResponse::Error {
                        message: format!("No pending pairing request from {}", telegram_id),
                    }
                }
            };

            let new_user = TelegramUser {
                telegram_id,
                name: pair.username.clone(),
                permissions: permissions.clone(),
                fs_allowed_paths: fs_allowed_paths.clone(),
                model: None,
            };

            {
                let mut cfg = config.write().await;
                // Remove if already exists, then add
                cfg.telegram.users.retain(|u| u.telegram_id != telegram_id);
                cfg.telegram.users.push(new_user);
                if let Err(e) = cfg.save().await {
                    return IpcResponse::Error {
                        message: format!("Failed to save config: {}", e),
                    };
                }
            }

            // Remove from pending
            pending.write().await.retain(|p| p.telegram_id != telegram_id);

            audit.log(AuditEvent::PairingDecision {
                telegram_id,
                approved: true,
                decided_by: "cli".to_string(),
            });

            info!(telegram_id, username = %pair.username, "User approved");
            IpcResponse::Ok {
                message: format!(
                    "User {} ({}) approved with permissions: {}",
                    pair.username,
                    telegram_id,
                    permissions.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                ),
            }
        }

        IpcRequest::PairReject { telegram_id } => {
            let removed = {
                let mut pairs = pending.write().await;
                let before = pairs.len();
                pairs.retain(|p| p.telegram_id != telegram_id);
                pairs.len() < before
            };

            if !removed {
                return IpcResponse::Error {
                    message: format!("No pending pairing request from {}", telegram_id),
                };
            }

            audit.log(AuditEvent::PairingDecision {
                telegram_id,
                approved: false,
                decided_by: "cli".to_string(),
            });

            IpcResponse::Ok {
                message: format!("Pairing request from {} rejected", telegram_id),
            }
        }

        IpcRequest::UsersList => {
            let cfg = config.read().await;
            let users = cfg
                .telegram
                .users
                .iter()
                .map(|u| UserInfo {
                    telegram_id: u.telegram_id,
                    name: u.name.clone(),
                    permissions: u.permissions.clone(),
                    fs_allowed_paths: u.fs_allowed_paths.clone(),
                })
                .collect();
            IpcResponse::UsersList { users }
        }

        IpcRequest::UserRemove { telegram_id } => {
            let removed = {
                let mut cfg = config.write().await;
                let before = cfg.telegram.users.len();
                cfg.telegram.users.retain(|u| u.telegram_id != telegram_id);
                let removed = cfg.telegram.users.len() < before;
                if removed {
                    if let Err(e) = cfg.save().await {
                        return IpcResponse::Error {
                            message: format!("Failed to save config: {}", e),
                        };
                    }
                }
                removed
            };

            if !removed {
                return IpcResponse::Error {
                    message: format!("User {} not found", telegram_id),
                };
            }

            sessions.reset_session(telegram_id).await;
            audit.log(AuditEvent::UserRemoved {
                telegram_id,
                removed_by: "cli".to_string(),
            });

            IpcResponse::Ok {
                message: format!("User {} removed", telegram_id),
            }
        }

        IpcRequest::Chat { message } => {
            // CLI chat: use a synthetic "CLI user" from config, or a fixed CLI identity
            let cli_user = {
                let cfg = config.read().await;
                // Use first user as CLI actor, or create a synthetic privileged one
                cfg.telegram.users.first().cloned().unwrap_or_else(|| TelegramUser {
                    telegram_id: 0,
                    name: "cli".to_string(),
                    permissions: vec![Permission::Fs],
                    fs_allowed_paths: vec![
                        std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
                    ],
                    model: None,
                })
            };

            match sessions.send_message(&cli_user, Channel::Cli, &message).await {
                Ok(reply) => IpcResponse::ChatReply { message: reply },
                Err(e) => IpcResponse::Error { message: e.to_string() },
            }
        }

        IpcRequest::ChatReset => {
            // Reset CLI session (user_id 0)
            sessions.reset_session(0).await;
            IpcResponse::Ok {
                message: "CLI session reset".to_string(),
            }
        }

        IpcRequest::Shutdown => {
            info!("Shutdown requested via IPC");
            // Spawn a task to exit after replying
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            IpcResponse::Ok {
                message: "Daemon shutting down".to_string(),
            }
        }
    }
}
