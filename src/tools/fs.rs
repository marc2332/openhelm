use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::fs;

use crate::ai::client::{FunctionDefinition, ToolDefinition};
use crate::permissions::Permission;
use super::{Tool, ToolContext, ToolOutput};

/// Resolve and validate that `raw_path` is inside one of `allowed_paths`.
/// Returns the canonicalized absolute path if allowed.
fn validate_path(raw_path: &str, allowed_paths: &[String]) -> Result<PathBuf> {
    let path = PathBuf::from(raw_path);

    // Expand ~ as home directory
    let path = if let Ok(stripped) = path.strip_prefix("~") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join(stripped)
    } else {
        path
    };

    // We canonicalize the parent dir (file might not exist yet for writes)
    let canonical = if path.exists() {
        dunce::canonicalize(&path).with_context(|| format!("Failed to resolve path: {}", path.display()))?
    } else if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            bail!("Cannot resolve relative path: {}", path.display());
        }
        let canonical_parent = dunce::canonicalize(parent)
            .with_context(|| format!("Parent directory does not exist: {}", parent.display()))?;
        canonical_parent.join(path.file_name().unwrap_or_default())
    } else {
        bail!("Invalid path: {}", path.display());
    };

    // Check against all allowed paths
    for allowed in allowed_paths {
        let allowed_path = PathBuf::from(allowed);
        let allowed_canonical = match dunce::canonicalize(&allowed_path) {
            Ok(p) => p,
            Err(_) => continue, // Skip non-existent allowed paths
        };
        if canonical.starts_with(&allowed_canonical) {
            return Ok(canonical);
        }
    }

    if allowed_paths.is_empty() {
        bail!(
            "Access denied: no allowed paths are configured for this user. \
            An administrator must set fs_allowed_paths in opencontrol.toml or re-approve with \
            `opencontrol pair approve <id> --allowed-paths /some/path`"
        );
    }

    bail!(
        "Access denied: '{}' is not within any allowed path. Allowed: {}",
        canonical.display(),
        allowed_paths.join(", ")
    );
}

// ─── fs_read ─────────────────────────────────────────────────────────────────

pub struct FsReadTool;

impl Tool for FsReadTool {
    fn name(&self) -> &'static str { "fs_read" }
    fn description(&self) -> &'static str { "Read the contents of a file" }
    fn required_permission(&self) -> Permission { Permission::Fs }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ relative path to the file to read"
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"]
                .as_str()
                .context("Missing or invalid 'path' argument")?;

            let path = validate_path(path_str, &context.allowed_paths)?;

            let contents = fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read file: {}", path.display()))?;

            Ok(ToolOutput {
                success: true,
                output: contents,
            })
        })
    }
}

// ─── fs_write ────────────────────────────────────────────────────────────────

pub struct FsWriteTool;

impl Tool for FsWriteTool {
    fn name(&self) -> &'static str { "fs_write" }
    fn description(&self) -> &'static str { "Write content to a file, creating it if it does not exist" }
    fn required_permission(&self) -> Permission { Permission::Fs }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ relative path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"]
                .as_str()
                .context("Missing or invalid 'path' argument")?;
            let content = args["content"]
                .as_str()
                .context("Missing or invalid 'content' argument")?;

            let path = validate_path(path_str, &context.allowed_paths)?;

            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
            }

            fs::write(&path, content)
                .await
                .with_context(|| format!("Failed to write file: {}", path.display()))?;

            Ok(ToolOutput {
                success: true,
                output: format!("Written {} bytes to {}", content.len(), path.display()),
            })
        })
    }
}

// ─── fs_list ─────────────────────────────────────────────────────────────────

pub struct FsListTool;

impl Tool for FsListTool {
    fn name(&self) -> &'static str { "fs_list" }
    fn description(&self) -> &'static str { "List the entries in a directory" }
    fn required_permission(&self) -> Permission { Permission::Fs }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ relative path to the directory to list"
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"]
                .as_str()
                .context("Missing or invalid 'path' argument")?;

            let path = validate_path(path_str, &context.allowed_paths)?;

            let mut entries = fs::read_dir(&path)
                .await
                .with_context(|| format!("Failed to list directory: {}", path.display()))?;

            let mut lines: Vec<String> = vec![];
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().to_string();
                let meta = entry.metadata().await?;
                let kind = if meta.is_dir() { "dir" } else { "file" };
                let size = if meta.is_file() {
                    format!(" ({} bytes)", meta.len())
                } else {
                    String::new()
                };
                lines.push(format!("[{}] {}{}", kind, name, size));
            }

            lines.sort();
            Ok(ToolOutput {
                success: true,
                output: if lines.is_empty() {
                    "(empty directory)".to_string()
                } else {
                    lines.join("\n")
                },
            })
        })
    }
}
