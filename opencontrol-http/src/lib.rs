use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

use opencontrol_sdk::{Skill, Tool, ToolDefinition, ToolOutput};

const MAX_BODY_BYTES: usize = 100 * 1024; // 100 KB

struct HttpClient(Client);

impl HttpClient {
    fn new() -> Self {
        let client = Client::builder()
            .user_agent("opencontrol-http/0.1.0")
            .build()
            .expect("Failed to build HTTP client");
        Self(client)
    }
}

fn url_arg(args: &Value) -> Result<&str> {
    args["url"]
        .as_str()
        .context("Missing required 'url' argument")
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    args: &Value,
) -> reqwest::RequestBuilder {
    if let Some(headers) = args.get("headers").and_then(|h| h.as_object()) {
        for (key, value) in headers {
            if let Some(v) = value.as_str() {
                builder = builder.header(key.as_str(), v);
            }
        }
    }
    builder
}

fn format_response(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap, body: &str) -> String {
    let mut out = format!("Status: {}\nHeaders:\n", status);
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            out.push_str(&format!("  {}: {}\n", name, v));
        }
    }
    out.push('\n');
    if body.len() > MAX_BODY_BYTES {
        out.push_str(&body[..MAX_BODY_BYTES]);
        out.push_str(&format!(
            "\n\n--- truncated ({} bytes total, showing first {}) ---",
            body.len(),
            MAX_BODY_BYTES
        ));
    } else {
        out.push_str(body);
    }
    out
}

fn format_head_response(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> String {
    let mut out = format!("Status: {}\nHeaders:\n", status);
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            out.push_str(&format!("  {}: {}\n", name, v));
        }
    }
    out
}

// ─── Tools ────────────────────────────────────────────────────────────────────

struct HttpGetTool(Arc<HttpClient>);

impl Tool for HttpGetTool {
    fn name(&self) -> &'static str {
        "http_get"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP GET request and return the response",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let builder = apply_headers(self.0 .0.get(url), args);
            let response = builder.send().await.context("HTTP GET request failed")?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.context("Failed to read response body")?;

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_response(status, &headers, &body),
            })
        })
    }
}

struct HttpPostTool(Arc<HttpClient>);

impl Tool for HttpPostTool {
    fn name(&self) -> &'static str {
        "http_post"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP POST request with an optional JSON body and return the response",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object",
                        "description": "Optional JSON body to send with the request"
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let mut builder = apply_headers(self.0 .0.post(url), args);
            if let Some(body) = args.get("body") {
                builder = builder.json(body);
            }
            let response = builder.send().await.context("HTTP POST request failed")?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.context("Failed to read response body")?;

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_response(status, &headers, &body),
            })
        })
    }
}

struct HttpPutTool(Arc<HttpClient>);

impl Tool for HttpPutTool {
    fn name(&self) -> &'static str {
        "http_put"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP PUT request with an optional JSON body and return the response",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object",
                        "description": "Optional JSON body to send with the request"
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let mut builder = apply_headers(self.0 .0.put(url), args);
            if let Some(body) = args.get("body") {
                builder = builder.json(body);
            }
            let response = builder.send().await.context("HTTP PUT request failed")?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.context("Failed to read response body")?;

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_response(status, &headers, &body),
            })
        })
    }
}

struct HttpPatchTool(Arc<HttpClient>);

impl Tool for HttpPatchTool {
    fn name(&self) -> &'static str {
        "http_patch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP PATCH request with an optional JSON body and return the response",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object",
                        "description": "Optional JSON body to send with the request"
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let mut builder = apply_headers(self.0 .0.patch(url), args);
            if let Some(body) = args.get("body") {
                builder = builder.json(body);
            }
            let response = builder.send().await.context("HTTP PATCH request failed")?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.context("Failed to read response body")?;

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_response(status, &headers, &body),
            })
        })
    }
}

struct HttpDeleteTool(Arc<HttpClient>);

impl Tool for HttpDeleteTool {
    fn name(&self) -> &'static str {
        "http_delete"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP DELETE request and return the response",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let builder = apply_headers(self.0 .0.delete(url), args);
            let response = builder.send().await.context("HTTP DELETE request failed")?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.context("Failed to read response body")?;

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_response(status, &headers, &body),
            })
        })
    }
}

struct HttpHeadTool(Arc<HttpClient>);

impl Tool for HttpHeadTool {
    fn name(&self) -> &'static str {
        "http_head"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Perform an HTTP HEAD request and return status and headers (no body)",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["url"]
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = url_arg(args)?;
            let builder = apply_headers(self.0 .0.head(url), args);
            let response = builder.send().await.context("HTTP HEAD request failed")?;
            let status = response.status();
            let headers = response.headers().clone();

            Ok(ToolOutput {
                success: status.is_success(),
                output: format_head_response(status, &headers),
            })
        })
    }
}

// ─── Skill ────────────────────────────────────────────────────────────────────

pub struct HttpSkill;

impl Skill for HttpSkill {
    fn name(&self) -> &'static str {
        "http"
    }

    fn build_tools(&self, _config: Option<&toml::Value>) -> Result<Vec<Box<dyn Tool>>> {
        let client = Arc::new(HttpClient::new());

        Ok(vec![
            Box::new(HttpGetTool(client.clone())),
            Box::new(HttpPostTool(client.clone())),
            Box::new(HttpPutTool(client.clone())),
            Box::new(HttpPatchTool(client.clone())),
            Box::new(HttpDeleteTool(client.clone())),
            Box::new(HttpHeadTool(client)),
        ])
    }
}
