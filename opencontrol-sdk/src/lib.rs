use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub use anyhow;
pub use serde_json;
pub use toml;

// ─── Tool primitives ──────────────────────────────────────────────────────────

/// The result of executing a tool.
pub struct ToolOutput {
    pub success: bool,
    pub output: String,
}

/// An OpenAI-compatible tool/function definition.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    /// Convenience constructor for function-type tools.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

// ─── Tool trait ───────────────────────────────────────────────────────────────

/// A tool that can be invoked by the AI.
/// Skill tools bake all required config in at construction time (via
/// [`Skill::build_tools`]) so they need no runtime context.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>>;
}

// ─── Skill trait ──────────────────────────────────────────────────────────────

/// A skill bundles a set of related tools and knows how to configure them.
pub trait Skill: Send + Sync {
    /// Unique lowercase identifier, e.g. `"github"`.
    fn name(&self) -> &'static str;

    /// Build pre-configured tool instances from the raw per-profile config
    /// table (`[profiles.<name>.skills.<skill>]`), or `None` if the table is
    /// absent.  Returns an error if required config (e.g. a token) is missing.
    fn build_tools(&self, config: Option<&toml::Value>) -> Result<Vec<Box<dyn Tool>>>;
}
