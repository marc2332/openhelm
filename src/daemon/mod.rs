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
    recv_request, send_response, IpcRequest, IpcResponse, PendingPair, ProfileInfo, UserInfo,
};
use crate::log_buffer::LogBuffer;
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
    log_buf: Arc<LogBuffer>,
}

impl Daemon {
    pub async fn new(config: Config, log_buf: Arc<LogBuffer>) -> Result<Self> {
        let audit = AuditLogger::new(&config.audit.log_path).await?;
        let client = AiClient::new(&config.ai.api_url, &config.ai.api_key)?;
        let tools = Arc::new(ToolRegistry::new());
        let timeout_minutes = config.ai.session_timeout_minutes;
        let sessions = Arc::new(SessionManager::new(
            client,
            tools,
            audit.clone(),
            timeout_minutes,
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
            log_buf,
        })
    }

    pub async fn run(self) -> Result<()> {
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
        let log_buf_arc = self.log_buf.clone();
        let start_time = self.start_time;

        tokio::spawn(async move {
            if let Err(e) = run_bot(bot_state).await {
                error!(error = %e, "Telegram bot error");
            }
        });

        // Session pruning — remove timed-out sessions every 60 seconds
        {
            let sessions_prune = sessions_arc.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let pruned = sessions_prune.prune_timed_out().await;
                    if pruned > 0 {
                        info!(count = pruned, "Pruned timed-out sessions");
                    }
                }
            });
        }

        let ipc_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let config = config_arc.clone();
                        let sessions = sessions_arc.clone();
                        let audit = audit_arc.clone();
                        let pending = pending_arc.clone();
                        let bot_connected = bot_connected_arc.clone();
                        let log_buf = log_buf_arc.clone();
                        tokio::spawn(handle_ipc_connection(
                            stream, config, sessions, audit, pending, bot_connected, log_buf, start_time,
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
    log_buf: Arc<LogBuffer>,
    start_time: Instant,
) {
    let req = match recv_request(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to read IPC request");
            return;
        }
    };

    let resp = dispatch(req, config, sessions, audit, pending, bot_connected, log_buf, start_time).await;

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
    log_buf: Arc<LogBuffer>,
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

        IpcRequest::PairApprove { telegram_id, profile } => {
            // Hard error if profile doesn't exist
            {
                let cfg = config.read().await;
                if let Err(e) = cfg.require_profile(&profile) {
                    return IpcResponse::Error { message: e.to_string() };
                }
            }

            let pair_info = {
                let pairs = pending.read().await;
                pairs.iter().find(|p| p.telegram_id == telegram_id).cloned()
            };

            let pair = match pair_info {
                Some(p) => p,
                None => return IpcResponse::Error {
                    message: format!("No pending pairing request from {}", telegram_id),
                },
            };

            let new_user = TelegramUser {
                telegram_id,
                name: pair.username.clone(),
                profile: profile.clone(),
            };

            {
                let mut cfg = config.write().await;
                cfg.telegram.users.retain(|u| u.telegram_id != telegram_id);
                cfg.telegram.users.push(new_user);
                if let Err(e) = cfg.save().await {
                    return IpcResponse::Error {
                        message: format!("Failed to save config: {}", e),
                    };
                }
            }

            pending.write().await.retain(|p| p.telegram_id != telegram_id);

            audit.log(AuditEvent::PairingDecision {
                telegram_id,
                approved: true,
                decided_by: "cli".to_string(),
            });

            info!(telegram_id, username = %pair.username, profile = %profile, "User approved");
            IpcResponse::Ok {
                message: format!(
                    "User {} ({}) approved with profile '{}'",
                    pair.username, telegram_id, profile
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
                    profile: u.profile.clone(),
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

        IpcRequest::ProfilesList => {
            let cfg = config.read().await;
            let profiles = cfg
                .profiles
                .iter()
                .map(|(name, p)| ProfileInfo {
                    name: name.clone(),
                    model: p.model.clone(),
                    has_custom_prompt: p.system_prompt.is_some(),
                    fs_enabled: p.permissions.fs,
                    fs: p.fs.clone(),
                })
                .collect();
            IpcResponse::ProfilesList { profiles }
        }

        IpcRequest::Chat { message, profile } => {
            // Validate profile exists
            let cfg_snapshot = config.read().await.clone();
            if let Err(e) = cfg_snapshot.require_profile(&profile) {
                return IpcResponse::Error { message: e.to_string() };
            }

            // Synthetic CLI user — telegram_id=0 is the reserved CLI user slot
            let cli_user = TelegramUser {
                telegram_id: 0,
                name: "cli".to_string(),
                profile,
            };

            match sessions.send_message(&cli_user, Channel::Cli, &message, &cfg_snapshot).await {
                Ok(reply) => IpcResponse::ChatReply { message: reply },
                Err(e) => IpcResponse::Error { message: e.to_string() },
            }
        }

        IpcRequest::ChatReset { profile } => {
            // Reset the synthetic CLI session for this profile.
            // All CLI sessions use telegram_id=0; reset by user_id.
            let _ = profile; // profile carried for future per-profile CLI sessions
            sessions.reset_session(0).await;
            IpcResponse::Ok { message: "CLI session reset".to_string() }
        }

        IpcRequest::Logs { lines, offset } => {
            let (log_lines, total) = if offset == 0 {
                log_buf.tail(lines)
            } else {
                log_buf.since(offset)
            };
            IpcResponse::Logs { lines: log_lines, total }
        }

        IpcRequest::Shutdown => {
            info!("Shutdown requested via IPC");
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            IpcResponse::Ok { message: "Daemon shutting down".to_string() }
        }
    }
}
