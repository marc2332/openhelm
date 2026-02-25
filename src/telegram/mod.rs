use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use teloxide::{
    prelude::*,
    types::ChatAction,
    utils::command::BotCommands,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::ai::session::SessionManager;
use crate::config::Config;
use crate::ipc::PendingPair;

/// Shared daemon state accessible from Telegram handlers.
#[derive(Clone)]
pub struct BotState {
    pub config: Arc<RwLock<Config>>,
    pub sessions: Arc<SessionManager>,
    pub audit: AuditLogger,
    pub pending_pairs: Arc<RwLock<Vec<PendingPair>>>,
    pub bot_connected: Arc<std::sync::atomic::AtomicBool>,
    #[allow(dead_code)]
    pub start_time: std::time::Instant,
}

/// Commands the bot understands.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum BotCommand {
    #[command(description = "Start using the bot or request pairing")]
    Start,
    #[command(description = "Show available commands")]
    Help,
    #[command(description = "Reset your conversation history")]
    Reset,
    #[command(description = "Show your current permissions")]
    Permissions,
}

/// Start the Telegram bot and run until cancelled.
pub async fn run_bot(state: BotState) -> Result<()> {
    let token = state.config.read().await.telegram.bot_token.clone();
    if token.is_empty() {
        warn!("No Telegram bot token configured — bot will not start");
        return Ok(());
    }

    let bot = Bot::new(token);

    // Validate the token before starting the dispatcher — a bad token causes
    // the dispatcher to panic, so we catch it here gracefully.
    match bot.get_me().await {
        Ok(me) => {
            info!(username = %me.username(), "Telegram bot connected");
            state.bot_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Err(e) => {
            warn!(error = %e, "Telegram bot token invalid or unreachable — bot will not start");
            return Ok(());
        }
    }

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<BotCommand>()
                .endpoint(command_handler),
        )
        .branch(Update::filter_message().endpoint(message_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// Handle slash commands.
async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: BotCommand,
    state: BotState,
) -> Result<(), teloxide::RequestError> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.clone())
        .unwrap_or_else(|| "unknown".to_string());

    match cmd {
        BotCommand::Start | BotCommand::Help => {
            let config = state.config.read().await;
            if config.find_user(user_id).is_some() {
                bot.send_message(
                    msg.chat.id,
                    "Welcome back! Send me a message and I'll help you.\n\
                    /reset — clear conversation history\n\
                    /permissions — show your permissions",
                )
                .await?;
            } else {
                // Not yet paired — log pairing request
                let already_pending = state
                    .pending_pairs
                    .read()
                    .await
                    .iter()
                    .any(|p| p.telegram_id == user_id);

                if !already_pending {
                    state.pending_pairs.write().await.push(PendingPair {
                        telegram_id: user_id,
                        username: username.clone(),
                        requested_at: Utc::now().to_rfc3339(),
                    });
                    state.audit.log(AuditEvent::PairingRequest {
                        telegram_id: user_id,
                        username: username.clone(),
                    });
                    info!(telegram_id = user_id, username = %username, "Pairing request received");
                }

                bot.send_message(
                    msg.chat.id,
                    "Hi! Your pairing request has been submitted.\n\
                    An administrator will approve your access shortly.\n\
                    Run `opencontrol pair list` on the server to see pending requests.",
                )
                .await?;
            }
        }

        BotCommand::Reset => {
            let config = state.config.read().await;
            if config.find_user(user_id).is_none() {
                bot.send_message(msg.chat.id, "You are not paired yet.").await?;
                return Ok(());
            }
            drop(config);
            state.sessions.reset_session(user_id).await;
            bot.send_message(msg.chat.id, "Conversation history cleared.").await?;
        }

        BotCommand::Permissions => {
            let config = state.config.read().await;
            if let Some(user) = config.find_user(user_id) {
                let perms: Vec<String> = user.permissions.iter().map(|p| p.to_string()).collect();
                let paths = user.fs_allowed_paths.join("\n  ");
                let text = if perms.is_empty() {
                    "You have no permissions assigned.".to_string()
                } else {
                    format!(
                        "Your permissions:\n{}\n\nAllowed paths:\n  {}",
                        perms.iter().map(|p| format!("• {}", p)).collect::<Vec<_>>().join("\n"),
                        if paths.is_empty() { "(none)".to_string() } else { paths }
                    )
                };
                bot.send_message(msg.chat.id, text).await?;
            } else {
                bot.send_message(msg.chat.id, "You are not paired yet.").await?;
            }
        }
    }

    Ok(())
}

/// Handle plain text messages.
async fn message_handler(
    bot: Bot,
    msg: Message,
    state: BotState,
) -> Result<(), teloxide::RequestError> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let text = match msg.text() {
        Some(t) => t.to_string(),
        None => return Ok(()),
    };

    let user = {
        let config = state.config.read().await;
        config.find_user(user_id).cloned()
    };

    let user = match user {
        Some(u) => u,
        None => {
            bot.send_message(
                msg.chat.id,
                "You are not paired yet. Send /start to request access.",
            )
            .await?;
            return Ok(());
        }
    };

    // Show typing indicator
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    match state
        .sessions
        .send_message(&user, Channel::Telegram, &text)
        .await
    {
        Ok(reply) => {
            // Send reply, splitting if over Telegram's 4096 char limit
            for chunk in split_message(&reply, 4096) {
                bot.send_message(msg.chat.id, chunk).await?;
            }
        }
        Err(e) => {
            error!(error = %e, user_id = user_id, "AI session error");
            bot.send_message(
                msg.chat.id,
                format!("An error occurred: {}", e),
            )
            .await?;
        }
    }

    Ok(())
}

/// Split a long message into chunks that fit within Telegram's limit.
fn split_message(text: &str, limit: usize) -> Vec<&str> {
    if text.len() <= limit {
        return vec![text];
    }
    let mut chunks = vec![];
    let mut start = 0;
    while start < text.len() {
        let end = (start + limit).min(text.len());
        // Try to break at a newline
        let slice = &text[start..end];
        let break_at = slice.rfind('\n').map(|i| i + 1).unwrap_or(slice.len());
        chunks.push(&text[start..start + break_at]);
        start += break_at;
    }
    chunks
}
