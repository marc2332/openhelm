use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use openhelm_sdk::{Skill, Tool, ToolDefinition, ToolOutput};

const DEFAULT_MAX_BODY_BYTES: usize = 15 * 1024 * 1024; // 15 MB

struct HttpClient {
    client: Client,
    max_body_bytes: usize,
}

impl HttpClient {
    fn new(max_body_bytes: usize) -> Self {
        let client = Client::builder()
            .user_agent("openhelm-http/0.1.0")
            .build()
            .expect("Failed to build HTTP client");
        Self {
            client,
            max_body_bytes,
        }
    }
}

fn url_arg(args: &Value) -> Result<&str> {
    args["url"]
        .as_str()
        .context("Missing required 'url' argument")
}

fn apply_headers(mut builder: reqwest::RequestBuilder, args: &Value) -> reqwest::RequestBuilder {
    if let Some(headers) = args.get("headers").and_then(|header| header.as_object()) {
        for (key, value) in headers {
            if let Some(val) = value.as_str() {
                builder = builder.header(key.as_str(), val);
            }
        }
    }
    builder
}

fn format_headers(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|val| format!("  {}: {}\n", name, val))
        })
        .collect()
}

fn format_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Option<&str>,
    max_body_bytes: usize,
) -> String {
    let mut out = format!("Status: {}\nHeaders:\n{}", status, format_headers(headers));
    if let Some(body) = body {
        out.push('\n');
        if body.len() > max_body_bytes {
            out.push_str(&body[..max_body_bytes]);
            out.push_str(&format!(
                "\n\n--- truncated ({} bytes total, showing first {}) ---",
                body.len(),
                max_body_bytes
            ));
        } else {
            out.push_str(body);
        }
    }
    out
}

struct HttpGetTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let builder = apply_headers(self.0.client.get(url), args);
        let response = builder.send().await.context("HTTP GET request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, Some(&body), self.0.max_body_bytes),
        })
    }
}

struct HttpPostTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let mut builder = apply_headers(self.0.client.post(url), args);
        if let Some(body) = args.get("body") {
            builder = builder.json(body);
        }
        let response = builder.send().await.context("HTTP POST request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, Some(&body), self.0.max_body_bytes),
        })
    }
}

struct HttpPutTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let mut builder = apply_headers(self.0.client.put(url), args);
        if let Some(body) = args.get("body") {
            builder = builder.json(body);
        }
        let response = builder.send().await.context("HTTP PUT request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, Some(&body), self.0.max_body_bytes),
        })
    }
}

struct HttpPatchTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let mut builder = apply_headers(self.0.client.patch(url), args);
        if let Some(body) = args.get("body") {
            builder = builder.json(body);
        }
        let response = builder.send().await.context("HTTP PATCH request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, Some(&body), self.0.max_body_bytes),
        })
    }
}

struct HttpDeleteTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let builder = apply_headers(self.0.client.delete(url), args);
        let response = builder.send().await.context("HTTP DELETE request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, Some(&body), self.0.max_body_bytes),
        })
    }
}

struct HttpHeadTool(Arc<HttpClient>);

#[async_trait]
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

    async fn execute(&self, args: &Value) -> Result<ToolOutput> {
        let url = url_arg(args)?;
        let builder = apply_headers(self.0.client.head(url), args);
        let response = builder.send().await.context("HTTP HEAD request failed")?;
        let status = response.status();
        let headers = response.headers().clone();

        Ok(ToolOutput {
            success: status.is_success(),
            output: format_response(status, &headers, None, 0),
        })
    }
}

pub struct HttpSkill;

#[async_trait]
impl Skill for HttpSkill {
    fn name(&self) -> &'static str {
        "http"
    }

    async fn build_tools(&self, config: Option<&toml::Value>) -> Result<Vec<Box<dyn Tool>>> {
        let max_body_bytes = config
            .and_then(|cfg| cfg.get("max_body_bytes"))
            .and_then(|val| val.as_integer())
            .map(|bytes| bytes as usize)
            .unwrap_or(DEFAULT_MAX_BODY_BYTES);

        let client = Arc::new(HttpClient::new(max_body_bytes));

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
