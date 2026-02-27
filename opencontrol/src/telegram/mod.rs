use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use telegram_markdown_v2::convert;
use teloxide::{
    prelude::*,
    types::{ChatAction, ParseMode},
    utils::command::BotCommands,
};
use tokio::sync::RwLock;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

use crate::ai::session::SessionManager;
use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::Config;
use crate::ipc::PendingPair;

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

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum BotCommand {
    #[command(description = "Start using the bot or request pairing")]
    Start,
    #[command(description = "Show available commands")]
    Help,
    #[command(description = "Reset your conversation history")]
    Reset,
    #[command(description = "Show your profile and permissions")]
    Profile,
}

/// Attempts to connect to Telegram API with exponential backoff retry logic.
///
/// Strategy:
/// - Attempt 1: Immediate
/// - Attempt 2-5: Wait 2s, 4s, 8s, 16s respectively (exponential backoff)
/// - Total max wait time: ~30 seconds across all retries
///
/// Returns: Ok(bot_info) on success, Err(error_msg) if all 5 attempts fail
async fn connect_with_retry(bot: &Bot) -> std::result::Result<teloxide::types::Me, String> {
    const MAX_RETRIES: usize = 5;

    for attempt in 1..=MAX_RETRIES {
        info!(
            attempt,
            total = MAX_RETRIES,
            "Attempting Telegram bot connection"
        );

        match bot.get_me().await {
            Ok(me) => return Ok(me),
            Err(e) => {
                let error_str = e.to_string();

                if attempt < MAX_RETRIES {
                    // Exponential backoff: 2^attempt seconds (2s, 4s, 8s, 16s)
                    let wait_seconds = 2_u64.pow(attempt as u32);
                    warn!(
                        attempt,
                        total = MAX_RETRIES,
                        error = %error_str,
                        retry_in_seconds = wait_seconds,
                        "Telegram connection failed, retrying"
                    );
                    sleep(Duration::from_secs(wait_seconds)).await;
                } else {
                    warn!(
                        attempt,
                        total = MAX_RETRIES,
                        error = %error_str,
                        "Telegram connection failed after all retries — bot will not start"
                    );
                    return Err(error_str);
                }
            }
        }
    }

    unreachable!()
}

pub async fn run_bot(state: BotState) -> Result<()> {
    let token = state.config.read().await.telegram.bot_token.clone();
    if token.is_empty() {
        warn!("No Telegram bot token configured — bot will not start");
        return Ok(());
    }

    let bot = Bot::new(token);

    // Attempt connection with exponential backoff retry logic
    match connect_with_retry(&bot).await {
        Ok(me) => {
            info!(username = %me.username(), "Telegram bot ready and waiting for messages");
            state
                .bot_connected
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Err(_) => {
            // Error already logged in connect_with_retry, gracefully degrade
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

async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: BotCommand,
    state: BotState,
) -> Result<(), teloxide::RequestError> {
    let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
    let username = msg
        .from
        .as_ref()
        .and_then(|user| user.username.clone())
        .unwrap_or_else(|| "unknown".to_string());

    match cmd {
        BotCommand::Start | BotCommand::Help => {
            let config = state.config.read().await;
            if config.find_user(user_id).is_some() {
                bot.send_message(
                    msg.chat.id,
                    "Welcome back! Send me a message and I'll help you.\n\
                    /reset — clear conversation history\n\
                    /profile — show your profile and permissions",
                )
                .await?;
            } else {
                let already_pending = state
                    .pending_pairs
                    .read()
                    .await
                    .iter()
                    .any(|pair| pair.telegram_id == user_id);

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
                bot.send_message(msg.chat.id, "You are not paired yet.")
                    .await?;
                return Ok(());
            }
            drop(config);
            state.sessions.reset_session(user_id).await;
            bot.send_message(msg.chat.id, "Conversation history cleared.")
                .await?;
        }

        BotCommand::Profile => {
            let config = state.config.read().await;
            if let Some(user) = config.find_user(user_id) {
                let profile_name = &user.profile;
                match config.resolve_profile(profile_name) {
                    Ok(profile) => {
                        let model = config.effective_model(user);
                        let mut lines = vec![
                            format!("Profile: {}", profile_name),
                            format!("Model:   {}", model),
                        ];

                        if profile.system_prompt.is_some() {
                            lines.push("System prompt: custom".to_string());
                        }

                        lines.push(String::new());
                        lines.push("Permissions:".to_string());

                        if profile.permissions.fs {
                            lines.push("  • fs (filesystem)".to_string());
                            if let Some(fs) = &profile.fs {
                                let fmt_paths = |paths: &Vec<String>| {
                                    if paths.is_empty() {
                                        "    (none)".to_string()
                                    } else {
                                        paths
                                            .iter()
                                            .map(|path| format!("    - {}", path))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    }
                                };
                                lines.push(format!("    read:\n{}", fmt_paths(&fs.read)));
                                lines.push(format!("    read_dir:\n{}", fmt_paths(&fs.read_dir)));
                                lines.push(format!("    write:\n{}", fmt_paths(&fs.write)));
                                lines.push(format!("    mkdir:\n{}", fmt_paths(&fs.mkdir)));
                            } else {
                                lines.push(
                                    "    (no paths configured — all operations denied)".to_string(),
                                );
                            }
                        } else {
                            lines.push("  (none)".to_string());
                        }

                        bot.send_message(msg.chat.id, lines.join("\n")).await?;
                    }
                    Err(err) => {
                        bot.send_message(msg.chat.id, format!("Profile error: {}", err))
                            .await?;
                    }
                }
            } else {
                bot.send_message(msg.chat.id, "You are not paired yet.")
                    .await?;
            }
        }
    }

    Ok(())
}

async fn message_handler(
    bot: Bot,
    msg: Message,
    state: BotState,
) -> Result<(), teloxide::RequestError> {
    let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
    let Some(text) = msg.text() else {
        return Ok(());
    };
    let text = text.to_string();

    let (user, config_snapshot) = {
        let config = state.config.read().await;
        let user = config.find_user(user_id).cloned();
        (user, config.clone())
    };

    let Some(user) = user else {
        bot.send_message(
            msg.chat.id,
            "You are not paired yet. Send /start to request access.",
        )
        .await?;
        return Ok(());
    };

    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    match state
        .sessions
        .send_message(&user, Channel::Telegram, &text, &config_snapshot)
        .await
    {
        Ok(reply) => match convert(&reply) {
            Ok(converted) => {
                for chunk in split_message(&converted, 4096) {
                    if !chunk.is_empty() {
                        bot.send_message(msg.chat.id, chunk)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to convert markdown, sending as plain text");
                for chunk in split_message(&reply, 4096) {
                    if !chunk.is_empty() {
                        bot.send_message(msg.chat.id, chunk).await?;
                    }
                }
            }
        },
        Err(err) => {
            error!(error = %err, user_id = user_id, "AI session error");
            let converted = convert(&format!("Error: {}", err))
                .unwrap_or_else(|_| "Error: Something went wrong.".to_string());
            bot.send_message(msg.chat.id, converted)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
    }

    Ok(())
}

fn split_message(text: &str, limit: usize) -> Vec<&str> {
    if text.len() <= limit {
        return vec![text];
    }
    let mut chunks = vec![];
    let mut start = 0;
    while start < text.len() {
        let end = (start + limit).min(text.len());
        let slice = &text[start..end];
        let break_at = slice.rfind('\n').map(|i| i + 1).unwrap_or(slice.len());
        chunks.push(&text[start..start + break_at]);
        start += break_at;
    }
    chunks
}
