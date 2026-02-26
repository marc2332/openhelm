use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;

use super::{Tool, ToolContext, ToolOutput};
use crate::ai::client::ToolDefinition;

fn resolve_path(raw_path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw_path);

    let path = if let Ok(stripped) = path.strip_prefix("~") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join(stripped)
    } else {
        path
    };

    if path.exists() {
        return dunce::canonicalize(&path)
            .with_context(|| format!("Failed to resolve path: {}", path.display()));
    }

    if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            bail!("Cannot resolve relative path: {}", path.display());
        }
        let canon_parent = dunce::canonicalize(parent)
            .with_context(|| format!("Parent directory does not exist: {}", parent.display()))?;
        return Ok(canon_parent.join(path.file_name().unwrap_or_default()));
    }

    bail!("Invalid path: {}", path.display());
}

fn check_allowed(path: &PathBuf, allowed: &[String], operation: &str) -> Result<()> {
    if allowed.is_empty() {
        bail!(
            "Operation '{}' is not permitted: no paths are configured for this operation in the profile. \
            Add paths under [profiles.<name>.fs].{} in opencontrol.toml",
            operation,
            operation
        );
    }

    for entry in allowed {
        let raw = if let Some(rest) = entry.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(entry)
        };

        let canon = dunce::canonicalize(&raw).unwrap_or(raw);

        if path.starts_with(&canon) {
            return Ok(());
        }
    }

    bail!(
        "Operation '{}' denied: '{}' is not within any allowed path.\nAllowed for {}: {}",
        operation,
        path.display(),
        operation,
        allowed.join(", ")
    );
}

pub struct FsReadTool;

impl Tool for FsReadTool {
    fn name(&self) -> &'static str {
        "fs_read"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Read the contents of a file",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or ~ relative path to the file to read"
                    }
                },
                "required": ["path"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"].as_str().context("Missing 'path' argument")?;
            let path = resolve_path(path_str)?;
            check_allowed(&path, &context.fs.read, "read")?;

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

pub struct FsWriteTool;

impl Tool for FsWriteTool {
    fn name(&self) -> &'static str {
        "fs_write"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Write content to a file, creating it if it does not exist",
            json!({
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
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"].as_str().context("Missing 'path' argument")?;
            let content = args["content"]
                .as_str()
                .context("Missing 'content' argument")?;
            let path = resolve_path(path_str)?;
            check_allowed(&path, &context.fs.write, "write")?;

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await.with_context(|| {
                    format!("Failed to create parent directory: {}", parent.display())
                })?;
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

pub struct FsListTool;

impl Tool for FsListTool {
    fn name(&self) -> &'static str {
        "fs_list"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "List the entries in a directory",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or ~ relative path to the directory to list"
                    }
                },
                "required": ["path"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"].as_str().context("Missing 'path' argument")?;
            let path = resolve_path(path_str)?;
            check_allowed(&path, &context.fs.read_dir, "read_dir")?;

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

pub struct FsMkdirTool;

impl Tool for FsMkdirTool {
    fn name(&self) -> &'static str {
        "fs_mkdir"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Create a directory (and any missing parents)",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or ~ relative path of the directory to create"
                    }
                },
                "required": ["path"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = args["path"].as_str().context("Missing 'path' argument")?;
            let path = resolve_path(path_str)?;
            check_allowed(&path, &context.fs.mkdir, "mkdir")?;

            fs::create_dir_all(&path)
                .await
                .with_context(|| format!("Failed to create directory: {}", path.display()))?;

            Ok(ToolOutput {
                success: true,
                output: format!("Created directory {}", path.display()),
            })
        })
    }
}
