use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::TelegramUser;
use crate::permissions::has_permission;
use crate::tools::{ToolContext, ToolRegistry};
use super::client::{AiClient, ChatMessage};

/// An AI session for a single user.
#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub username: String,
    pub channel: Channel,
    pub history: Vec<ChatMessage>,
    pub model: String,
    last_activity: std::time::Instant,
    timeout_minutes: u64,
}

impl Session {
    pub fn new(
        user: &TelegramUser,
        channel: Channel,
        model: impl Into<String>,
        _system_prompt: &str,
        timeout_minutes: u64,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            user_id: user.telegram_id,
            username: user.name.clone(),
            channel,
            history: vec![],
            model: model.into(),
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
        info!(session_id = %self.id, user_id = self.user_id, "Session history reset");
    }

    pub fn touch(&mut self) {
        self.last_activity = std::time::Instant::now();
    }
}

/// Manager that holds all active sessions.
pub struct SessionManager {
    sessions: Mutex<Vec<Session>>,
    client: AiClient,
    tools: Arc<ToolRegistry>,
    audit: AuditLogger,
    default_model: String,
    system_prompt: String,
    timeout_minutes: u64,
}

impl SessionManager {
    pub fn new(
        client: AiClient,
        tools: Arc<ToolRegistry>,
        audit: AuditLogger,
        default_model: impl Into<String>,
        system_prompt: impl Into<String>,
        timeout_minutes: u64,
    ) -> Self {
        Self {
            sessions: Mutex::new(vec![]),
            client,
            tools,
            audit,
            default_model: default_model.into(),
            system_prompt: system_prompt.into(),
            timeout_minutes,
        }
    }

    /// Get or create a session for a user, returning the session id.
    async fn get_or_create_session_id(
        &self,
        user: &TelegramUser,
        channel: Channel,
    ) -> String {
        let mut sessions = self.sessions.lock().await;

        if let Some(s) = sessions.iter_mut().find(|s| s.user_id == user.telegram_id) {
            if s.is_timed_out() {
                info!(user_id = user.telegram_id, "Session timed out, resetting");
                s.reset();
            }
            s.touch();
            return s.id.clone();
        }

        let model = user.model.clone().unwrap_or_else(|| self.default_model.clone());
        let session = Session::new(user, channel, model, &self.system_prompt, self.timeout_minutes);
        let id = session.id.clone();

        self.audit.log(AuditEvent::SessionStart {
            user_id: user.telegram_id,
            username: user.name.clone(),
            channel,
            session_id: id.clone(),
        });

        sessions.push(session);
        id
    }

    /// Send a user message and run the agentic tool loop, returning the final reply.
    pub async fn send_message(
        &self,
        user: &TelegramUser,
        channel: Channel,
        user_message: &str,
    ) -> Result<String> {
        let session_id = self.get_or_create_session_id(user, channel).await;

        let model = user.model.clone().unwrap_or_else(|| self.default_model.clone());

        // Log message
        let preview = user_message.chars().take(100).collect::<String>();
        self.audit.log(AuditEvent::MessageSent {
            user_id: user.telegram_id,
            session_id: session_id.clone(),
            preview,
            model: model.clone(),
        });

        let tool_defs = self.tools.definitions_for(&user.permissions);
        let tool_context = ToolContext {
            user_id: user.telegram_id,
            allowed_paths: user.fs_allowed_paths.clone(),
        };

        // Push user message
        {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|s| s.id == session_id)
                .expect("Session must exist");
            session.history.push(ChatMessage::user(user_message));
        }

        // Agentic loop — we pull the history out, release the lock, make the API
        // call, then re-acquire to update history.
        loop {
            // Snapshot history (lock acquired and immediately released)
            let history_snapshot = {
                let sessions = self.sessions.lock().await;
                let session = sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .expect("Session must exist");
                session.history.clone()
            };

            let mut messages = vec![ChatMessage::system(&self.system_prompt)];
            messages.extend(history_snapshot);

            // Call AI API without holding the lock
            let resp = self
                .client
                .chat(&model, &messages, Some(&tool_defs))
                .await?;

            let choice = resp.choices.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("No choices in AI response")
            })?;

            let assistant_msg = choice.message;
            let finish_reason = choice.finish_reason.as_deref().unwrap_or("stop");
            let tool_calls = assistant_msg.tool_calls.clone().unwrap_or_default();

            // Push assistant message to history
            {
                let mut sessions = self.sessions.lock().await;
                let session = sessions
                    .iter_mut()
                    .find(|s| s.id == session_id)
                    .expect("Session must exist");
                session.history.push(assistant_msg.clone());
            }

            // If no tool calls or stop, return content
            if finish_reason == "stop" || tool_calls.is_empty() {
                let mut sessions = self.sessions.lock().await;
                let session = sessions
                    .iter_mut()
                    .find(|s| s.id == session_id)
                    .expect("Session must exist");
                session.touch();
                return Ok(assistant_msg.content.unwrap_or_default());
            }

            debug!(count = tool_calls.len(), "Processing tool calls");

            // Process each tool call outside the lock
            for tc in &tool_calls {
                let tool_name = &tc.function.name;
                let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::Value::Object(Default::default()));

                let tool = self.tools.find(tool_name);
                let allowed = match &tool {
                    Some(t) => has_permission(&user.permissions, &t.required_permission()),
                    None => false,
                };

                self.audit.log(AuditEvent::ToolCall {
                    user_id: user.telegram_id,
                    session_id: session_id.clone(),
                    tool: tool_name.clone(),
                    args: args.clone(),
                    allowed,
                });

                let result_content = if !allowed {
                    warn!(tool = tool_name, user_id = user.telegram_id, "Tool call denied");
                    format!("Error: Permission denied for tool '{}'", tool_name)
                } else if let Some(t) = tool {
                    // Execute tool without holding the lock
                    match t.execute(&args, &tool_context).await {
                        Ok(output) => {
                            self.audit.log(AuditEvent::ToolResult {
                                user_id: user.telegram_id,
                                session_id: session_id.clone(),
                                tool: tool_name.clone(),
                                success: output.success,
                                error: None,
                            });
                            output.output
                        }
                        Err(e) => {
                            let err = e.to_string();
                            self.audit.log(AuditEvent::ToolResult {
                                user_id: user.telegram_id,
                                session_id: session_id.clone(),
                                tool: tool_name.clone(),
                                success: false,
                                error: Some(err.clone()),
                            });
                            format!("Error: {}", err)
                        }
                    }
                } else {
                    format!("Error: Unknown tool '{}'", tool_name)
                };

                // Push tool result to history
                {
                    let mut sessions = self.sessions.lock().await;
                    let session = sessions
                        .iter_mut()
                        .find(|s| s.id == session_id)
                        .expect("Session must exist");
                    session.history.push(ChatMessage::tool_result(&tc.id, result_content));
                }
            }

            // Continue loop so the model can process tool results
        }
    }

    /// Reset a user's session history.
    pub async fn reset_session(&self, user_id: i64) {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.iter_mut().find(|s| s.user_id == user_id) {
            s.reset();
        }
    }

    /// Return count of active sessions.
    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}
