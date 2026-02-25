use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{Config, TelegramUser};
use crate::tools::{ToolContext, ToolRegistry};
use super::client::{AiClient, ChatMessage};

/// An in-memory AI conversation for a single user.
#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub username: String,
    pub channel: Channel,
    history: Vec<ChatMessage>,
    last_activity: std::time::Instant,
    timeout_minutes: u64,
}

impl Session {
    pub fn new(user: &TelegramUser, channel: Channel, timeout_minutes: u64) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
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
        info!(session_id = %self.id, user_id = self.user_id, "Session history reset");
    }

    pub fn touch(&mut self) {
        self.last_activity = std::time::Instant::now();
    }
}

/// Manages all active in-memory sessions.
pub struct SessionManager {
    sessions: Mutex<Vec<Session>>,
    client: AiClient,
    tools: Arc<ToolRegistry>,
    audit: AuditLogger,
    timeout_minutes: u64,
}

impl SessionManager {
    pub fn new(
        client: AiClient,
        tools: Arc<ToolRegistry>,
        audit: AuditLogger,
        timeout_minutes: u64,
    ) -> Self {
        Self {
            sessions: Mutex::new(vec![]),
            client,
            tools,
            audit,
            timeout_minutes,
        }
    }

    /// Get or create a session id for a user.
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

        let session = Session::new(user, channel, self.timeout_minutes);
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
    /// `config` is passed here (not stored) so profile changes take effect without restart.
    pub async fn send_message(
        &self,
        user: &TelegramUser,
        channel: Channel,
        user_message: &str,
        config: &Config,
    ) -> Result<String> {
        // Resolve profile — error immediately if missing
        let profile = config.resolve_profile(&user.profile)?;
        let model = config.effective_model(user);
        let system_prompt = config.effective_system_prompt(user);

        let session_id = self.get_or_create_session_id(user, channel).await;

        let preview = user_message.chars().take(100).collect::<String>();
        self.audit.log(AuditEvent::MessageSent {
            user_id: user.telegram_id,
            session_id: session_id.clone(),
            preview,
            model: model.clone(),
        });

        let tool_defs = self.tools.definitions_for(profile);
        let tool_context = ToolContext::from_profile(profile);

        // Push user message
        {
            let mut sessions = self.sessions.lock().await;
            sessions
                .iter_mut()
                .find(|s| s.id == session_id)
                .expect("session must exist")
                .history
                .push(ChatMessage::user(user_message));
        }

        // Agentic loop
        loop {
            // Snapshot history without holding the lock
            let history_snapshot = {
                let sessions = self.sessions.lock().await;
                sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .expect("session must exist")
                    .history
                    .clone()
            };

            let mut messages = vec![ChatMessage::system(&system_prompt)];
            messages.extend(history_snapshot);

            let resp = self.client.chat(&model, &messages, Some(&tool_defs)).await?;

            let choice = resp
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("No choices in AI response"))?;

            let assistant_msg = choice.message;
            let finish_reason = choice.finish_reason.as_deref().unwrap_or("stop");
            let tool_calls = assistant_msg.tool_calls.clone().unwrap_or_default();

            // Append assistant message
            {
                let mut sessions = self.sessions.lock().await;
                sessions
                    .iter_mut()
                    .find(|s| s.id == session_id)
                    .expect("session must exist")
                    .history
                    .push(assistant_msg.clone());
            }

            if finish_reason == "stop" || tool_calls.is_empty() {
                let mut sessions = self.sessions.lock().await;
                sessions
                    .iter_mut()
                    .find(|s| s.id == session_id)
                    .expect("session must exist")
                    .touch();
                return Ok(assistant_msg.content.unwrap_or_default());
            }

            debug!(count = tool_calls.len(), "Processing tool calls");

            for tc in &tool_calls {
                let tool_name = &tc.function.name;
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                let found = self.tools.find(tool_name);

                // Check group-level enablement
                let allowed = match &found {
                    Some((_, crate::tools::ToolGroup::Fs)) => profile.permissions.fs,
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
                    if found.is_none() {
                        format!("Error: Unknown tool '{}'", tool_name)
                    } else {
                        format!("Error: Tool '{}' is not enabled in your profile", tool_name)
                    }
                } else {
                    let tool = found.unwrap().0;
                    match tool.execute(&args, &tool_context).await {
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
                };

                {
                    let mut sessions = self.sessions.lock().await;
                    sessions
                        .iter_mut()
                        .find(|s| s.id == session_id)
                        .expect("session must exist")
                        .history
                        .push(ChatMessage::tool_result(&tc.id, result_content));
                }
            }
        }
    }

    pub async fn reset_session(&self, user_id: i64) {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.iter_mut().find(|s| s.user_id == user_id) {
            s.reset();
        }
    }

    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}
