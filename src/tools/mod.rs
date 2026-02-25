pub mod fs;

use anyhow::Result;
use serde_json::Value;

use crate::ai::client::ToolDefinition;
use crate::permissions::Permission;

/// Result of executing a tool.
pub struct ToolOutput {
    pub success: bool,
    pub output: String,
}

/// A tool that can be invoked by the AI.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn required_permission(&self) -> Permission;
    fn definition(&self) -> ToolDefinition;
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>>;
}

/// Contextual information passed to every tool call.
pub struct ToolContext {
    #[allow(dead_code)]
    pub user_id: i64,
    pub allowed_paths: Vec<String>,
}

/// Registry of all available tools.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(fs::FsReadTool),
            Box::new(fs::FsWriteTool),
            Box::new(fs::FsListTool),
        ];
        Self { tools }
    }

    /// Return tool definitions for tools the user is permitted to use.
    pub fn definitions_for(&self, permissions: &[Permission]) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| permissions.contains(&t.required_permission()))
            .map(|t| t.definition())
            .collect()
    }

    /// Find a tool by name.
    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
