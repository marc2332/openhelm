use std::sync::Arc;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use rig::completion::message::{ImageMediaType, UserContent};
use telegram_markdown_v2::convert;
use teloxide::types::ParseMode;
use teloxide::{net::Download, prelude::*, types::ChatAction, utils::command::BotCommands};
use tokio::sync::RwLock;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

use crate::ai::session::SessionManager;
use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{AttachmentsConfig, Config, IMAGE_EXTENSIONS};
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
                    Run `openhelm pair list` on the server to see pending requests.",
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

/// The result of extracting content from a Telegram message.
enum MessageContent {
    /// Successfully extracted content parts to send to the AI.
    Parts(Vec<UserContent>),
    /// An error message to send back to the user (e.g. unsupported file type).
    UserError(String),
    /// The message type is completely unhandled (stickers, contacts, etc.).
    Unsupported,
}

/// Extract text and/or attachment content from a Telegram message.
///
/// Returns `MessageContent::Parts` with one or more `UserContent` items when
/// the message can be processed, `MessageContent::UserError` when we can
/// identify the problem and give a helpful reply, or `MessageContent::Unsupported`
/// for message types we can't handle at all.
async fn extract_message_content(
    bot: &Bot,
    msg: &Message,
    attachments_config: Option<&AttachmentsConfig>,
) -> MessageContent {
    let text = msg.text().map(|t| t.to_string());
    let caption = msg.caption().map(|c| c.to_string());
    let effective_text = text.or(caption);

    // Photo messages
    if let Some(photos) = msg.photo() {
        let att_cfg = match attachments_config {
            Some(cfg) if cfg.enabled => cfg,
            _ => {
                // Attachments disabled: process caption as plain text if present
                return match effective_text {
                    Some(t) => MessageContent::Parts(vec![UserContent::text(t)]),
                    None => MessageContent::UserError(
                        "File attachments are not enabled for your profile.".to_string(),
                    ),
                };
            }
        };

        // Photos from the camera button have no file extension. We treat them
        // as JPEG and require an image extension in the allowed list.
        let has_image_ext = att_cfg
            .allowed_extensions
            .iter()
            .any(|ext| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()));

        if !has_image_ext {
            return match effective_text {
                Some(t) => {
                    // Process caption, inform about the skipped photo
                    MessageContent::Parts(vec![UserContent::text(format!(
                        "{}\n\n[A photo was attached but image types are not allowed. Allowed types: {}]",
                        t,
                        att_cfg.allowed_extensions.join(", ")
                    ))])
                }
                None => MessageContent::UserError(format!(
                    "Image files are not allowed. Allowed file types: {}",
                    att_cfg.allowed_extensions.join(", ")
                )),
            };
        }

        // Pick the largest photo size
        let photo = photos.iter().max_by_key(|p| p.width * p.height);
        let photo = match photo {
            Some(p) => p,
            None => return MessageContent::UserError("Could not read the photo.".to_string()),
        };

        // Check file size
        if u64::from(photo.file.size) > att_cfg.max_file_size_bytes {
            let size_mb = photo.file.size as f64 / (1024.0 * 1024.0);
            let max_mb = att_cfg.max_file_size_bytes as f64 / (1024.0 * 1024.0);
            return MessageContent::UserError(format!(
                "Photo too large ({:.1} MB). Maximum allowed: {:.1} MB.",
                size_mb, max_mb
            ));
        }

        // Download the photo
        match download_telegram_file(bot, &photo.file.id).await {
            Ok(data) => {
                let b64 = BASE64.encode(&data);
                let mut parts = Vec::new();
                if let Some(t) = effective_text {
                    parts.push(UserContent::text(t));
                }
                parts.push(UserContent::image_base64(
                    b64,
                    Some(ImageMediaType::JPEG),
                    None,
                ));
                return MessageContent::Parts(parts);
            }
            Err(e) => {
                error!(error = %e, "Failed to download photo from Telegram");
                return MessageContent::UserError(
                    "Failed to download the photo. Please try again.".to_string(),
                );
            }
        }
    }

    // Document messages
    if let Some(doc) = msg.document() {
        let att_cfg = match attachments_config {
            Some(cfg) if cfg.enabled => cfg,
            _ => {
                // Attachments disabled: process caption as plain text if present
                return match effective_text {
                    Some(t) => MessageContent::Parts(vec![UserContent::text(t)]),
                    None => MessageContent::UserError(
                        "File attachments are not enabled for your profile.".to_string(),
                    ),
                };
            }
        };

        let file_name = doc.file_name.as_deref().unwrap_or("unknown");
        let extension = file_name.rsplit('.').next().unwrap_or("").to_lowercase();

        // Check if extension is allowed
        let is_allowed = att_cfg
            .allowed_extensions
            .iter()
            .any(|ext| ext.to_lowercase() == extension);

        if !is_allowed {
            return match effective_text {
                Some(t) => MessageContent::Parts(vec![UserContent::text(format!(
                    "{}\n\n[A file '{}' was attached but '.{}' is not allowed. Allowed types: {}]",
                    t,
                    file_name,
                    extension,
                    att_cfg.allowed_extensions.join(", ")
                ))]),
                None => MessageContent::UserError(format!(
                    "File type '.{}' is not allowed. Allowed types: {}",
                    extension,
                    att_cfg.allowed_extensions.join(", ")
                )),
            };
        }

        // Check file size
        if u64::from(doc.file.size) > att_cfg.max_file_size_bytes {
            let size_mb = doc.file.size as f64 / (1024.0 * 1024.0);
            let max_mb = att_cfg.max_file_size_bytes as f64 / (1024.0 * 1024.0);
            return MessageContent::UserError(format!(
                "File '{}' too large ({:.1} MB). Maximum allowed: {:.1} MB.",
                file_name, size_mb, max_mb
            ));
        }

        // Download the file
        let data = match download_telegram_file(bot, &doc.file.id).await {
            Ok(d) => d,
            Err(e) => {
                error!(error = %e, file_name = file_name, "Failed to download file from Telegram");
                return MessageContent::UserError(
                    "Failed to download the file. Please try again.".to_string(),
                );
            }
        };

        let is_image = IMAGE_EXTENSIONS.contains(&extension.as_str());

        if is_image {
            // Send as image
            let media_type = match extension.as_str() {
                "jpg" | "jpeg" => Some(ImageMediaType::JPEG),
                "png" => Some(ImageMediaType::PNG),
                "gif" => Some(ImageMediaType::GIF),
                "webp" => Some(ImageMediaType::WEBP),
                _ => None,
            };
            let b64 = BASE64.encode(&data);
            let mut parts = Vec::new();
            if let Some(t) = effective_text {
                parts.push(UserContent::text(t));
            }
            parts.push(UserContent::image_base64(b64, media_type, None));
            return MessageContent::Parts(parts);
        }

        // Text-based file: try to decode as UTF-8
        match String::from_utf8(data) {
            Ok(content) => {
                let mut parts = Vec::new();
                if let Some(t) = effective_text {
                    parts.push(UserContent::text(t));
                }
                parts.push(UserContent::text(format!(
                    "File `{}`:\n```\n{}\n```",
                    file_name, content
                )));
                return MessageContent::Parts(parts);
            }
            Err(_) => {
                return MessageContent::UserError(format!(
                    "Could not read '{}' as text — it may be a binary file.",
                    file_name
                ));
            }
        }
    }

    // Plain text messages
    if let Some(t) = effective_text {
        return MessageContent::Parts(vec![UserContent::text(t)]);
    }

    // Everything else (voice, sticker, video, contact, location, etc.)
    MessageContent::Unsupported
}

/// Download a file from Telegram by its `file_id`.
async fn download_telegram_file(bot: &Bot, file_id: &str) -> Result<Vec<u8>> {
    let file = bot
        .get_file(file_id)
        .await
        .map_err(|e| anyhow::anyhow!("get_file failed: {}", e))?;
    let mut buf = Vec::new();
    bot.download_file(&file.path, &mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("download_file failed: {}", e))?;
    Ok(buf)
}

async fn message_handler(
    bot: Bot,
    msg: Message,
    state: BotState,
) -> Result<(), teloxide::RequestError> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

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

    // Resolve attachment config for this user's profile
    let attachments_config = config_snapshot
        .resolve_profile(&user.profile)
        .ok()
        .and_then(|p| p.attachments.clone());

    let content = extract_message_content(&bot, &msg, attachments_config.as_ref()).await;

    let parts = match content {
        MessageContent::Parts(parts) => parts,
        MessageContent::UserError(err_msg) => {
            bot.send_message(msg.chat.id, err_msg).await?;
            return Ok(());
        }
        MessageContent::Unsupported => {
            bot.send_message(
                msg.chat.id,
                "Unsupported message type. Send text or a supported file.",
            )
            .await?;
            return Ok(());
        }
    };

    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    match state
        .sessions
        .send_message(&user, Channel::Telegram, parts, &config_snapshot)
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
