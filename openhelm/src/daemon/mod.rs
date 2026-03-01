use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

use anyhow::{Context, Result};
use rig::completion::message::UserContent;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::ai::client::AiClient;
use crate::ai::session::{SessionEvent, SessionManager};
use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{Config, TelegramUser};
use crate::ipc::{
    IpcRequest, IpcResponse, PendingPair, ProfileInfo, UserInfo, recv_request, send_response,
};
use crate::log_buffer::LogBuffer;
use crate::telegram::{BotState, run_bot};
use crate::tools::SkillRegistry;

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

/// Shared daemon state passed to every IPC connection handler.
#[derive(Clone)]
struct DaemonContext {
    config: Arc<RwLock<Config>>,
    sessions: Arc<SessionManager>,
    audit: AuditLogger,
    pending: Arc<RwLock<Vec<PendingPair>>>,
    bot_connected: Arc<AtomicBool>,
    log_buf: Arc<LogBuffer>,
    start_time: Instant,
}

impl Daemon {
    pub async fn new(config: Config, log_buf: Arc<LogBuffer>) -> Result<Self> {
        let audit = AuditLogger::new(&config.audit.log_path).await?;
        let client = AiClient::new(&config.ai)?;
        let skills = Arc::new(SkillRegistry::new());
        let timeout_minutes = config.ai.session_timeout_minutes;
        let sessions = Arc::new(SessionManager::new(
            client,
            skills,
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

        tokio::spawn(async move {
            if let Err(err) = run_bot(bot_state).await {
                error!(error = %err, "Telegram bot error");
            }
        });

        tokio::spawn({
            let sessions = self.sessions.clone();
            async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let pruned = sessions.prune_timed_out().await;
                    if pruned > 0 {
                        info!(count = pruned, "Pruned timed-out sessions");
                    }
                }
            }
        });

        let ctx = DaemonContext {
            config: self.config,
            sessions: self.sessions,
            audit: self.audit,
            pending: self.pending_pairs,
            bot_connected: self.bot_connected,
            log_buf: self.log_buf,
            start_time: self.start_time,
        };

        let ipc_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(handle_ipc_connection(stream, ctx.clone()));
                    }
                    Err(err) => {
                        error!(error = %err, "Failed to accept IPC connection");
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

async fn handle_ipc_connection(mut stream: UnixStream, ctx: DaemonContext) {
    let req = match recv_request(&mut stream).await {
        Ok(request) => request,
        Err(err) => {
            warn!(error = %err, "Failed to read IPC request");
            return;
        }
    };

    // Chat requests are handled specially: they stream multiple IPC
    // responses (ChatChunk / ChatDone) over the socket instead of a
    // single response.
    if let IpcRequest::Chat { message, profile } = req {
        handle_chat_streaming(&mut stream, message, profile, ctx.config, ctx.sessions).await;
        return;
    }

    let resp = dispatch(req, ctx).await;

    if let Err(err) = send_response(&mut stream, &resp).await {
        warn!(error = %err, "Failed to send IPC response");
    }
}

/// Handle a Chat request by streaming events over the socket.
async fn handle_chat_streaming(
    stream: &mut UnixStream,
    message: String,
    profile: String,
    config: Arc<RwLock<Config>>,
    sessions: Arc<SessionManager>,
) {
    let cfg_snapshot = config.read().await.clone();
    if let Err(err) = cfg_snapshot.require_profile(&profile) {
        let _ = send_response(
            stream,
            &IpcResponse::Error {
                message: err.to_string(),
            },
        )
        .await;
        return;
    }

    let cli_user = TelegramUser {
        telegram_id: 0,
        name: "cli".to_string(),
        profile,
    };

    let mut rx = match sessions
        .send_message(
            &cli_user,
            Channel::Cli,
            vec![UserContent::text(message)],
            &cfg_snapshot,
        )
        .await
    {
        Ok(rx) => rx,
        Err(err) => {
            let _ = send_response(
                stream,
                &IpcResponse::Error {
                    message: err.to_string(),
                },
            )
            .await;
            return;
        }
    };

    // Drain the session event channel, forwarding events as streaming
    // IPC responses.
    while let Some(event) = rx.recv().await {
        let resp = match event {
            SessionEvent::Typing => continue,
            SessionEvent::Chunk(text) => IpcResponse::ChatChunk { text },
            SessionEvent::Done(_) => IpcResponse::ChatDone,
            SessionEvent::Error(err) => IpcResponse::Error {
                message: err.to_string(),
            },
        };

        if send_response(stream, &resp).await.is_err() {
            return;
        }

        // After ChatDone or Error we're finished.
        if matches!(resp, IpcResponse::ChatDone | IpcResponse::Error { .. }) {
            return;
        }
    }

    // Channel closed without a Done event - send ChatDone as a safety net.
    let _ = send_response(stream, &IpcResponse::ChatDone).await;
}

async fn dispatch(req: IpcRequest, ctx: DaemonContext) -> IpcResponse {
    let DaemonContext {
        config,
        sessions,
        audit,
        pending,
        bot_connected,
        log_buf,
        start_time,
    } = ctx;
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
            profile,
        } => {
            {
                let cfg = config.read().await;
                if let Err(err) = cfg.require_profile(&profile) {
                    return IpcResponse::Error {
                        message: err.to_string(),
                    };
                }
            }

            let pair_info = {
                let pairs = pending.read().await;
                pairs
                    .iter()
                    .find(|pair| pair.telegram_id == telegram_id)
                    .cloned()
            };

            let Some(pair) = pair_info else {
                return IpcResponse::Error {
                    message: format!("No pending pairing request from {}", telegram_id),
                };
            };

            let new_user = TelegramUser {
                telegram_id,
                name: pair.username.clone(),
                profile: profile.clone(),
            };

            {
                let mut cfg = config.write().await;
                cfg.telegram
                    .users
                    .retain(|user| user.telegram_id != telegram_id);
                cfg.telegram.users.push(new_user);
                if let Err(err) = cfg.save().await {
                    return IpcResponse::Error {
                        message: format!("Failed to save config: {}", err),
                    };
                }
            }

            pending
                .write()
                .await
                .retain(|pair| pair.telegram_id != telegram_id);

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
                pairs.retain(|pair| pair.telegram_id != telegram_id);
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
                .map(|user| UserInfo {
                    telegram_id: user.telegram_id,
                    name: user.name.clone(),
                    profile: user.profile.clone(),
                })
                .collect();
            IpcResponse::UsersList { users }
        }

        IpcRequest::UserRemove { telegram_id } => {
            let removed = {
                let mut cfg = config.write().await;
                let before = cfg.telegram.users.len();
                cfg.telegram
                    .users
                    .retain(|user| user.telegram_id != telegram_id);
                let removed = cfg.telegram.users.len() < before;
                if removed && let Err(err) = cfg.save().await {
                    return IpcResponse::Error {
                        message: format!("Failed to save config: {}", err),
                    };
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
                .map(|(name, profile)| ProfileInfo {
                    name: name.clone(),
                    model: profile.model.clone(),
                    has_custom_prompt: profile.system_prompt.is_some(),
                    fs_enabled: profile.permissions.fs,
                    fs: profile.fs.clone(),
                })
                .collect();
            IpcResponse::ProfilesList { profiles }
        }

        // Chat requests are handled before dispatch() via
        // handle_chat_streaming(), so this arm is unreachable.
        IpcRequest::Chat { .. } => unreachable!("Chat handled in handle_ipc_connection"),

        IpcRequest::ChatReset { profile } => {
            let _ = profile;
            sessions.reset_session(0).await;
            IpcResponse::Ok {
                message: "CLI session reset".to_string(),
            }
        }

        IpcRequest::Logs { lines, offset } => {
            let (log_lines, total) = if offset == 0 {
                log_buf.tail(lines)
            } else {
                log_buf.since(offset)
            };
            IpcResponse::Logs {
                lines: log_lines,
                total,
            }
        }

        IpcRequest::Shutdown => {
            info!("Shutdown requested via IPC");
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
