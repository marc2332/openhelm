use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

use super::client::AiClient;
use crate::ai::client::{FinishReason, StreamEvent};
use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{Config, TelegramUser};
use crate::tools::{SkillRegistry, ToolContext, ToolRegistry};
use rig::completion::Message;
use rig::completion::message::UserContent;
use rig::one_or_many::OneOrMany;

/// Events streamed from the agentic loop back to the caller.
pub enum SessionEvent {
    /// Signal that the AI is still working - caller should refresh a typing indicator.
    Typing,
    /// A token-level text chunk from the model - deliver/display immediately.
    Chunk(String),
    /// Intermediate text produced by the AI alongside tool calls - deliver immediately.
    /// Kept for backwards-compat; used when the model emits text *before* tool calls.
    Message(String),
    /// The final reply. May be empty when all content was already sent as `Chunk`
    /// or `Message` events.
    Done(String),
    /// A fatal error terminated the loop.
    Error(anyhow::Error),
}

pub struct Session {
    pub user_id: i64,
    #[allow(dead_code)]
    pub username: String,
    #[allow(dead_code)]
    pub channel: Channel,
    history: Vec<Message>,
    last_activity: std::time::Instant,
    timeout_minutes: u64,
}

impl Session {
    pub fn new(user: &TelegramUser, channel: Channel, timeout_minutes: u64) -> Self {
        Self {
            user_id: user.telegram_id,
            username: user.name.clone(),
            channel,
            history: vec![],
            last_activity: std::time::Instant::now(),
            timeout_minutes,
        }
    }

    pub fn is_timed_out(&self) -> bool {
        self.last_activity.elapsed().as_secs() > self.timeout_minutes * 60
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.last_activity = std::time::Instant::now();
        info!(user_id = self.user_id, "Session history reset");
    }

    pub fn touch(&mut self) {
        self.last_activity = std::time::Instant::now();
    }
}

/// Manages all active in-memory sessions.
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<i64, Session>>>,
    client: AiClient,
    skills: Arc<SkillRegistry>,
    audit: AuditLogger,
    timeout_minutes: u64,
}

impl SessionManager {
    pub fn new(
        client: AiClient,
        skills: Arc<SkillRegistry>,
        audit: AuditLogger,
        timeout_minutes: u64,
    ) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            client,
            skills,
            audit,
            timeout_minutes,
        }
    }

    async fn ensure_session(&self, user: &TelegramUser, channel: Channel) -> bool {
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&user.telegram_id) {
                if session.is_timed_out() {
                    info!(user_id = user.telegram_id, "Session timed out, resetting");
                    session.reset();
                }
                session.touch();
                return false;
            }
        }

        let session = Session::new(user, channel, self.timeout_minutes);
        self.sessions
            .write()
            .await
            .insert(user.telegram_id, session);
        true
    }

    /// Send a message and get back a channel receiver that streams [`SessionEvent`]s.
    ///
    /// The agentic loop runs in a spawned task. The caller should drain the
    /// receiver until it's closed, handling each event as appropriate:
    ///
    /// - [`SessionEvent::Typing`]    – refresh a typing/progress indicator.
    /// - [`SessionEvent::Chunk`]     – a token-level text delta; display live.
    /// - [`SessionEvent::Message`]   – intermediate text; deliver to the user now.
    /// - [`SessionEvent::Done`]      – final reply (empty when all text was already
    ///                                 sent as `Chunk`/`Message` events).
    /// - [`SessionEvent::Error`]     – fatal error; the loop has stopped.
    pub async fn send_message(
        &self,
        user: &TelegramUser,
        channel: Channel,
        content: Vec<UserContent>,
        config: &Config,
    ) -> Result<mpsc::Receiver<SessionEvent>> {
        let profile = config.resolve_profile(&user.profile)?;
        let model = config.effective_model(user);
        let system_prompt = config.effective_system_prompt(user);

        let tools = ToolRegistry::for_profile(profile, &self.skills).await?;

        let is_new = self.ensure_session(user, channel).await;
        if is_new {
            self.audit.log(AuditEvent::SessionStart {
                user_id: user.telegram_id,
                username: user.name.clone(),
                channel,
                session_id: user.telegram_id.to_string(),
            });
        }

        // Build a preview string for logging/audit from the first text content part
        let preview = content
            .iter()
            .find_map(|c| {
                if let UserContent::Text(t) = c {
                    Some(t.text.chars().take(100).collect::<String>())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "[attachment]".to_string());

        info!(
            user_id = user.telegram_id,
            username = %user.name,
            profile = %user.profile,
            channel = ?channel,
            preview = %preview,
            "Message received"
        );
        self.audit.log(AuditEvent::MessageSent {
            user_id: user.telegram_id,
            session_id: user.telegram_id.to_string(),
            preview,
            model: model.clone(),
        });

        let tool_defs = tools.definitions_for(profile);
        let tool_context = ToolContext::from_profile(profile);
        // Clone the profile data needed for permission checks in the spawned task.
        let profile_permissions = profile.permissions.clone();

        // Build the user message from content parts
        let user_msg = Message::User {
            content: OneOrMany::many(content)
                .map_err(|_| anyhow::anyhow!("Empty message content"))?,
        };

        self.sessions
            .write()
            .await
            .get_mut(&user.telegram_id)
            .expect("session must exist")
            .history
            .push(user_msg);

        // Clone the pieces the spawned task needs.
        let sessions = self.sessions.clone();
        let client = self.client.clone();
        let audit = self.audit.clone();
        let user = user.clone();

        let (tx, rx) = mpsc::channel::<SessionEvent>(64);

        tokio::spawn(async move {
            debug!(user_id = user.telegram_id, "Agentic loop task started");
            loop {
                // Signal "still thinking" so the caller can refresh a typing indicator.
                if tx.send(SessionEvent::Typing).await.is_err() {
                    warn!(
                        user_id = user.telegram_id,
                        "Receiver dropped, aborting agentic loop"
                    );
                    return;
                }

                let history_snapshot = sessions
                    .read()
                    .await
                    .get(&user.telegram_id)
                    .expect("session must exist")
                    .history
                    .clone();

                let mut messages = vec![Message::user(&system_prompt)];
                messages.extend(history_snapshot);

                // ── Use the streaming API ──────────────────────────────────
                let mut stream_rx = match client
                    .chat_stream(&model, &messages, Some(&tool_defs))
                    .await
                {
                    Ok(rx) => rx,
                    Err(err) => {
                        warn!(user_id = user.telegram_id, error = %err, "AI stream call failed");
                        let _ = tx.send(SessionEvent::Error(err)).await;
                        return;
                    }
                };

                // Drain the stream, forwarding text chunks to the caller and
                // collecting tool calls + text for history.
                let mut accumulated_text = String::new();
                let mut tool_calls: Vec<crate::ai::client::ToolCall> = Vec::new();
                let mut finish_reason: Option<FinishReason> = None;

                while let Some(event) = stream_rx.recv().await {
                    match event {
                        StreamEvent::TextDelta(delta) => {
                            accumulated_text.push_str(&delta);
                            // Forward the chunk live to the caller.
                            if tx.send(SessionEvent::Chunk(delta)).await.is_err() {
                                return;
                            }
                        }
                        StreamEvent::ToolCall(tc) => {
                            tool_calls.push(tc);
                        }
                        StreamEvent::Done { finish_reason: fr } => {
                            finish_reason = fr;
                        }
                        StreamEvent::Error(msg) => {
                            let _ = tx
                                .send(SessionEvent::Error(anyhow::anyhow!("{}", msg)))
                                .await;
                            return;
                        }
                    }
                }

                let text_content: Option<String> =
                    if accumulated_text.is_empty() || accumulated_text == "[tool call]" {
                        None
                    } else {
                        Some(accumulated_text)
                    };

                debug!(
                    user_id = user.telegram_id,
                    tool_call_count = tool_calls.len(),
                    has_text = text_content.is_some(),
                    "AI streaming response received"
                );

                if let Some(ref text) = text_content {
                    sessions
                        .write()
                        .await
                        .get_mut(&user.telegram_id)
                        .expect("session must exist")
                        .history
                        .push(Message::assistant(text));
                }

                // Terminate the loop when the model is done: either an
                // explicit stop / length-limit, or no tool calls to process.
                let is_done = match finish_reason {
                    Some(FinishReason::Stop) | Some(FinishReason::Length) => true,
                    _ => tool_calls.is_empty(),
                };

                if is_done {
                    sessions
                        .write()
                        .await
                        .get_mut(&user.telegram_id)
                        .expect("session must exist")
                        .touch();

                    let reply = text_content.unwrap_or_default();
                    let reply_preview = reply.chars().take(100).collect::<String>();
                    info!(
                        user_id = user.telegram_id,
                        username = %user.name,
                        profile = %user.profile,
                        channel = ?channel,
                        finish_reason = ?finish_reason,
                        preview = %reply_preview,
                        reply_len = reply.len(),
                        "Sending Done event"
                    );
                    // Text was already streamed as Chunk events, so send
                    // Done with empty string (the caller already has all the text).
                    let _ = tx.send(SessionEvent::Done(String::new())).await;
                    return;
                }

                // There are tool calls - emit any intermediate text as a
                // Message event so old callers that don't handle Chunk see it.
                // (Text was already streamed chunk-by-chunk, so callers that
                // handle Chunk will have already displayed it.)
                if let Some(text) = text_content {
                    if tx.send(SessionEvent::Message(text)).await.is_err() {
                        return;
                    }
                }

                debug!(count = tool_calls.len(), "Processing tool calls");

                for tc in &tool_calls {
                    let tool_name = &tc.function.name;
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                    let found = tools.find(tool_name);

                    let allowed = match &found {
                        Some((_, crate::tools::ToolGroup::Fs)) => profile_permissions.fs,
                        Some((_, crate::tools::ToolGroup::Skill(skill_name))) => {
                            profile_permissions.skills.contains_key(*skill_name)
                        }
                        None => false,
                    };

                    audit.log(AuditEvent::ToolCall {
                        user_id: user.telegram_id,
                        session_id: user.telegram_id.to_string(),
                        tool: tool_name.clone(),
                        args: args.clone(),
                        allowed,
                    });

                    let result_content = if !allowed {
                        warn!(
                            tool = tool_name,
                            user_id = user.telegram_id,
                            "Tool call denied"
                        );
                        if found.is_none() {
                            format!("Error: Unknown tool '{}'", tool_name)
                        } else {
                            format!("Error: Tool '{}' is not enabled in your profile", tool_name)
                        }
                    } else {
                        let (tool, _group) = found.expect("allowed implies found");
                        match tool.execute(&args, &tool_context).await {
                            Ok(output) => {
                                audit.log(AuditEvent::ToolResult {
                                    user_id: user.telegram_id,
                                    session_id: user.telegram_id.to_string(),
                                    tool: tool_name.clone(),
                                    success: output.success,
                                    error: None,
                                });
                                output.output
                            }
                            Err(err) => {
                                let err = err.to_string();
                                audit.log(AuditEvent::ToolResult {
                                    user_id: user.telegram_id,
                                    session_id: user.telegram_id.to_string(),
                                    tool: tool_name.clone(),
                                    success: false,
                                    error: Some(err.clone()),
                                });
                                format!("Error: {}", err)
                            }
                        }
                    };

                    sessions
                        .write()
                        .await
                        .get_mut(&user.telegram_id)
                        .expect("session must exist")
                        .history
                        .push(Message::user(format!(
                            "Tool '{}' result: {}",
                            tc.function.name, result_content
                        )));
                }
            }
        });

        Ok(rx)
    }

    pub async fn reset_session(&self, user_id: i64) {
        if let Some(session) = self.sessions.write().await.get_mut(&user_id) {
            session.reset();
        }
    }

    pub async fn prune_timed_out(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, session| !session.is_timed_out());
        before - sessions.len()
    }

    pub async fn active_count(&self) -> usize {
        self.sessions
            .read()
            .await
            .values()
            .filter(|session| !session.is_timed_out())
            .count()
    }
}
