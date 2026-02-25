use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::audit::{AuditEvent, AuditLogger, Channel};
use crate::config::{Config, TelegramUser};
use crate::tools::{SkillRegistry, ToolContext, ToolRegistry};
use super::client::{AiClient, ChatMessage};

pub struct Session {
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
    sessions: RwLock<HashMap<i64, Session>>,
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
            sessions: RwLock::new(HashMap::new()),
            client,
            skills,
            audit,
            timeout_minutes,
        }
    }

    async fn ensure_session(&self, user: &TelegramUser, channel: Channel) -> bool {
        {
            let mut sessions = self.sessions.write().await;
            if let Some(s) = sessions.get_mut(&user.telegram_id) {
                if s.is_timed_out() {
                    info!(user_id = user.telegram_id, "Session timed out, resetting");
                    s.reset();
                }
                s.touch();
                return false;
            }
        }

        let session = Session::new(user, channel, self.timeout_minutes);
        self.sessions.write().await.insert(user.telegram_id, session);
        true
    }

    pub async fn send_message(
        &self,
        user: &TelegramUser,
        channel: Channel,
        user_message: &str,
        config: &Config,
    ) -> Result<String> {
        let profile = config.resolve_profile(&user.profile)?;
        let model = config.effective_model(user);
        let system_prompt = config.effective_system_prompt(user);

        let tools = ToolRegistry::for_profile(profile, &self.skills)?;

        let is_new = self.ensure_session(user, channel).await;
        if is_new {
            self.audit.log(AuditEvent::SessionStart {
                user_id: user.telegram_id,
                username: user.name.clone(),
                channel,
                session_id: user.telegram_id.to_string(),
            });
        }

        let preview = user_message.chars().take(100).collect::<String>();
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

        self.sessions.write().await
            .get_mut(&user.telegram_id)
            .expect("session must exist")
            .history
            .push(ChatMessage::user(user_message));

        loop {
            let history_snapshot = self.sessions.read().await
                .get(&user.telegram_id)
                .expect("session must exist")
                .history
                .clone();

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
            let tool_calls = assistant_msg.tool_calls.as_deref().unwrap_or(&[]);

            self.sessions.write().await
                .get_mut(&user.telegram_id)
                .expect("session must exist")
                .history
                .push(assistant_msg.clone());

            if finish_reason == "stop" || tool_calls.is_empty() {
                self.sessions.write().await
                    .get_mut(&user.telegram_id)
                    .expect("session must exist")
                    .touch();
                let reply = assistant_msg.content.unwrap_or_default();
                let reply_preview = reply.chars().take(100).collect::<String>();
                info!(
                    user_id = user.telegram_id,
                    username = %user.name,
                    profile = %user.profile,
                    channel = ?channel,
                    preview = %reply_preview,
                    "Reply sent"
                );
                return Ok(reply);
            }

            debug!(count = tool_calls.len(), "Processing tool calls");

            for tc in tool_calls {
                let tool_name = &tc.function.name;
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                let found = tools.find(tool_name);

                let allowed = match &found {
                    Some((_, crate::tools::ToolGroup::Fs)) => profile.permissions.fs,
                    Some((_, crate::tools::ToolGroup::Skill(skill_name))) => {
                        profile.permissions.skills.contains_key(*skill_name)
                    }
                    None => false,
                };

                self.audit.log(AuditEvent::ToolCall {
                    user_id: user.telegram_id,
                    session_id: user.telegram_id.to_string(),
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
                    let (tool, _group) = found.expect("allowed implies found");
                    match tool.execute(&args, &tool_context).await {
                        Ok(output) => {
                            self.audit.log(AuditEvent::ToolResult {
                                user_id: user.telegram_id,
                                session_id: user.telegram_id.to_string(),
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
                                session_id: user.telegram_id.to_string(),
                                tool: tool_name.clone(),
                                success: false,
                                error: Some(err.clone()),
                            });
                            format!("Error: {}", err)
                        }
                    }
                };

                self.sessions.write().await
                    .get_mut(&user.telegram_id)
                    .expect("session must exist")
                    .history
                    .push(ChatMessage::tool_result(&tc.id, result_content));
            }
        }
    }

    pub async fn reset_session(&self, user_id: i64) {
        if let Some(s) = self.sessions.write().await.get_mut(&user_id) {
            s.reset();
        }
    }

    pub async fn prune_timed_out(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, s| !s.is_timed_out());
        before - sessions.len()
    }

    pub async fn active_count(&self) -> usize {
        self.sessions.read().await.values().filter(|s| !s.is_timed_out()).count()
    }
}
