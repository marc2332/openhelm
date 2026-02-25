pub mod fs;

use anyhow::Result;
use serde_json::Value;

use crate::ai::client::ToolDefinition;
use crate::config::{FsPermissions, Profile};

/// Result of executing a tool.
pub struct ToolOutput {
    pub success: bool,
    pub output: String,
}

/// A tool that can be invoked by the AI.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>>;
}

/// Contextual permissions passed to every tool call for this session.
pub struct ToolContext {
    pub fs: FsPermissions,
}

impl ToolContext {
    pub fn from_profile(profile: &Profile) -> Self {
        Self {
            fs: profile.fs.clone().unwrap_or_default(),
        }
    }
}

/// Registry of all available tools.
pub struct ToolRegistry {
    fs_tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            fs_tools: vec![
                Box::new(fs::FsReadTool),
                Box::new(fs::FsWriteTool),
                Box::new(fs::FsListTool),
                Box::new(fs::FsMkdirTool),
            ],
        }
    }

    /// Return tool definitions enabled by the given profile.
    pub fn definitions_for(&self, profile: &Profile) -> Vec<ToolDefinition> {
        let mut defs = vec![];
        if profile.permissions.fs {
            defs.extend(self.fs_tools.iter().map(|t| t.definition()));
        }
        defs
    }

    /// Find a tool by name, also returning which group it belongs to so the
    /// caller can check group-level enablement.
    pub fn find(&self, name: &str) -> Option<(&dyn Tool, ToolGroup)> {
        if let Some(t) = self.fs_tools.iter().find(|t| t.name() == name) {
            return Some((t.as_ref(), ToolGroup::Fs));
        }
        None
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGroup {
    Fs,
}
