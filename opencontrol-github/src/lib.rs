use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{json, Value};

use opencontrol_sdk::{Skill, Tool, ToolDefinition, ToolOutput};

// ─── GitHub HTTP client ───────────────────────────────────────────────────────

#[derive(Clone)]
struct GithubClient {
    http: Client,
    token: String,
}

impl GithubClient {
    fn new(token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("opencontrol")
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self { http, token: token.into() })
    }

    fn get(&self, path: &str) -> RequestBuilder {
        let url = format!("https://api.github.com{}", path);
        self.http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let resp = self.get(path).send().await
            .with_context(|| format!("GitHub API request failed: GET {}", path))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GitHub API returned {}: {}", status, body);
        }
        resp.json::<Value>().await.context("Failed to parse GitHub API response")
    }
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

fn repo_arg(args: &Value) -> Result<String> {
    let repo = args["repo"].as_str().context("Missing 'repo' argument (format: owner/repo)")?;
    if !repo.contains('/') {
        bail!("'repo' must be in 'owner/repo' format, got: {}", repo);
    }
    Ok(repo.to_string())
}

// ─── Tool: github_get_repo ────────────────────────────────────────────────────

struct GithubGetRepoTool(Arc<GithubClient>);

impl Tool for GithubGetRepoTool {
    fn name(&self) -> &'static str { "github_get_repo" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Get metadata for a GitHub repository (description, stars, forks, default branch, open issues count, topics, license, visibility)",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    }
                },
                "required": ["repo"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let data = self.0.get_json(&format!("/repos/{}", repo)).await?;

            let output = format!(
                "Repo:          {}\nDescription:   {}\nVisibility:    {}\nDefault branch:{}\nStars:         {}\nForks:         {}\nOpen issues:   {}\nTopics:        {}\nLicense:       {}\nURL:           {}",
                data["full_name"].as_str().unwrap_or("-"),
                data["description"].as_str().unwrap_or("(none)"),
                data["visibility"].as_str().unwrap_or("-"),
                data["default_branch"].as_str().unwrap_or("-"),
                data["stargazers_count"].as_u64().unwrap_or(0),
                data["forks_count"].as_u64().unwrap_or(0),
                data["open_issues_count"].as_u64().unwrap_or(0),
                data["topics"].as_array()
                    .map(|t| t.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                data["license"]["name"].as_str().unwrap_or("(none)"),
                data["html_url"].as_str().unwrap_or("-"),
            );

            Ok(ToolOutput { success: true, output })
        })
    }
}

// ─── Tool: github_list_issues ─────────────────────────────────────────────────

struct GithubListIssuesTool(Arc<GithubClient>);

impl Tool for GithubListIssuesTool {
    fn name(&self) -> &'static str { "github_list_issues" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "List issues for a GitHub repository",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    },
                    "state": {
                        "type": "string",
                        "enum": ["open", "closed", "all"],
                        "description": "Issue state filter (default: open)"
                    },
                    "label": {
                        "type": "string",
                        "description": "Filter by label name"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of issues to return (default: 20, max: 100)"
                    }
                },
                "required": ["repo"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let state = args["state"].as_str().unwrap_or("open");
            let limit = args["limit"].as_u64().unwrap_or(20).min(100);

            let mut path = format!("/repos/{}/issues?state={}&per_page={}&pulls=false", repo, state, limit);
            if let Some(label) = args["label"].as_str() {
                path.push_str(&format!("&labels={}", label));
            }

            let data = self.0.get_json(&path).await?;
            let issues = data.as_array().context("Expected array of issues")?;

            if issues.is_empty() {
                return Ok(ToolOutput { success: true, output: "No issues found.".to_string() });
            }

            let lines: Vec<String> = issues.iter()
                .filter(|i| i["pull_request"].is_null()) // exclude PRs from issues endpoint
                .map(|i| format!(
                    "#{} [{}] {} ({})",
                    i["number"].as_u64().unwrap_or(0),
                    i["state"].as_str().unwrap_or("?"),
                    i["title"].as_str().unwrap_or("(no title)"),
                    i["user"]["login"].as_str().unwrap_or("?"),
                ))
                .collect();

            Ok(ToolOutput { success: true, output: lines.join("\n") })
        })
    }
}

// ─── Tool: github_get_issue ───────────────────────────────────────────────────

struct GithubGetIssueTool(Arc<GithubClient>);

impl Tool for GithubGetIssueTool {
    fn name(&self) -> &'static str { "github_get_issue" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Get the full details of a GitHub issue including body and comments",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    },
                    "number": {
                        "type": "integer",
                        "description": "Issue number"
                    }
                },
                "required": ["repo", "number"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let number = args["number"].as_u64().context("Missing 'number' argument")?;

            let issue = self.0.get_json(&format!("/repos/{}/issues/{}", repo, number)).await?;
            let comments = self.0.get_json(&format!("/repos/{}/issues/{}/comments", repo, number)).await?;

            let mut out = format!(
                "#{} [{}] {}\nAuthor: {}\nCreated: {}\nURL: {}\n\n{}\n",
                issue["number"].as_u64().unwrap_or(0),
                issue["state"].as_str().unwrap_or("?"),
                issue["title"].as_str().unwrap_or("(no title)"),
                issue["user"]["login"].as_str().unwrap_or("?"),
                issue["created_at"].as_str().unwrap_or("?"),
                issue["html_url"].as_str().unwrap_or("?"),
                issue["body"].as_str().unwrap_or("(no body)"),
            );

            if let Some(comment_list) = comments.as_array() {
                if !comment_list.is_empty() {
                    out.push_str(&format!("\n--- {} comment(s) ---\n", comment_list.len()));
                    for c in comment_list {
                        out.push_str(&format!(
                            "\n[{}] {}:\n{}\n",
                            c["created_at"].as_str().unwrap_or("?"),
                            c["user"]["login"].as_str().unwrap_or("?"),
                            c["body"].as_str().unwrap_or("(empty)"),
                        ));
                    }
                }
            }

            Ok(ToolOutput { success: true, output: out })
        })
    }
}

// ─── Tool: github_list_prs ────────────────────────────────────────────────────

struct GithubListPrsTool(Arc<GithubClient>);

impl Tool for GithubListPrsTool {
    fn name(&self) -> &'static str { "github_list_prs" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "List pull requests for a GitHub repository",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    },
                    "state": {
                        "type": "string",
                        "enum": ["open", "closed", "all"],
                        "description": "PR state filter (default: open)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of PRs to return (default: 20, max: 100)"
                    }
                },
                "required": ["repo"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let state = args["state"].as_str().unwrap_or("open");
            let limit = args["limit"].as_u64().unwrap_or(20).min(100);

            let path = format!("/repos/{}/pulls?state={}&per_page={}", repo, state, limit);
            let data = self.0.get_json(&path).await?;
            let prs = data.as_array().context("Expected array of PRs")?;

            if prs.is_empty() {
                return Ok(ToolOutput { success: true, output: "No pull requests found.".to_string() });
            }

            let lines: Vec<String> = prs.iter()
                .map(|pr| format!(
                    "#{} [{}] {} ({} → {}) by {}",
                    pr["number"].as_u64().unwrap_or(0),
                    pr["state"].as_str().unwrap_or("?"),
                    pr["title"].as_str().unwrap_or("(no title)"),
                    pr["head"]["ref"].as_str().unwrap_or("?"),
                    pr["base"]["ref"].as_str().unwrap_or("?"),
                    pr["user"]["login"].as_str().unwrap_or("?"),
                ))
                .collect();

            Ok(ToolOutput { success: true, output: lines.join("\n") })
        })
    }
}

// ─── Tool: github_get_pr ──────────────────────────────────────────────────────

struct GithubGetPrTool(Arc<GithubClient>);

impl Tool for GithubGetPrTool {
    fn name(&self) -> &'static str { "github_get_pr" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Get details of a GitHub pull request including description, diff stats, and review comments",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    },
                    "number": {
                        "type": "integer",
                        "description": "Pull request number"
                    }
                },
                "required": ["repo", "number"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let number = args["number"].as_u64().context("Missing 'number' argument")?;

            let pr = self.0.get_json(&format!("/repos/{}/pulls/{}", repo, number)).await?;
            let comments = self.0.get_json(&format!("/repos/{}/issues/{}/comments", repo, number)).await?;

            let mut out = format!(
                "#{} [{}] {}\nAuthor:  {}\nBranch:  {} → {}\nCreated: {}\nURL:     {}\nChanges: +{} -{} in {} file(s)\n\n{}\n",
                pr["number"].as_u64().unwrap_or(0),
                pr["state"].as_str().unwrap_or("?"),
                pr["title"].as_str().unwrap_or("(no title)"),
                pr["user"]["login"].as_str().unwrap_or("?"),
                pr["head"]["ref"].as_str().unwrap_or("?"),
                pr["base"]["ref"].as_str().unwrap_or("?"),
                pr["created_at"].as_str().unwrap_or("?"),
                pr["html_url"].as_str().unwrap_or("?"),
                pr["additions"].as_u64().unwrap_or(0),
                pr["deletions"].as_u64().unwrap_or(0),
                pr["changed_files"].as_u64().unwrap_or(0),
                pr["body"].as_str().unwrap_or("(no description)"),
            );

            if let Some(comment_list) = comments.as_array() {
                if !comment_list.is_empty() {
                    out.push_str(&format!("\n--- {} comment(s) ---\n", comment_list.len()));
                    for c in comment_list {
                        out.push_str(&format!(
                            "\n[{}] {}:\n{}\n",
                            c["created_at"].as_str().unwrap_or("?"),
                            c["user"]["login"].as_str().unwrap_or("?"),
                            c["body"].as_str().unwrap_or("(empty)"),
                        ));
                    }
                }
            }

            Ok(ToolOutput { success: true, output: out })
        })
    }
}

// ─── Tool: github_get_file ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ContentResponse {
    content: Option<String>,
    encoding: Option<String>,
    message: Option<String>,
}

struct GithubGetFileTool(Arc<GithubClient>);

impl Tool for GithubGetFileTool {
    fn name(&self) -> &'static str { "github_get_file" }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Get the raw contents of a file from a GitHub repository",
            json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository in 'owner/repo' format"
                    },
                    "path": {
                        "type": "string",
                        "description": "Path to the file within the repository"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Branch, tag, or commit SHA (default: repo's default branch)"
                    }
                },
                "required": ["repo", "path"]
            }),
        )
    }

    fn execute<'a>(&'a self, args: &'a Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let repo = repo_arg(args)?;
            let path = args["path"].as_str().context("Missing 'path' argument")?;
            let mut api_path = format!("/repos/{}/contents/{}", repo, path);
            if let Some(r) = args["ref"].as_str() {
                api_path.push_str(&format!("?ref={}", r));
            }

            let raw = self.0.get_json(&api_path).await?;
            let resp: ContentResponse =
                serde_json::from_value(raw).context("Failed to parse contents response")?;

            if let Some(msg) = resp.message {
                bail!("GitHub API error: {}", msg);
            }

            let content = resp.content.context("No content in response")?;
            let encoding = resp.encoding.as_deref().unwrap_or("none");

            if encoding == "base64" {
                // GitHub returns base64 with newlines — strip them before decoding
                let cleaned: String = content.chars().filter(|c| *c != '\n').collect();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&cleaned)
                    .context("Failed to decode base64 content")?;
                let text = String::from_utf8(decoded)
                    .context("File content is not valid UTF-8 — binary file?")?;
                Ok(ToolOutput { success: true, output: text })
            } else {
                Ok(ToolOutput { success: true, output: content })
            }
        })
    }
}

// ─── GithubSkill ──────────────────────────────────────────────────────────────

pub struct GithubSkill;

impl Skill for GithubSkill {
    fn name(&self) -> &'static str { "github" }

    fn build_tools(&self, config: Option<&toml::Value>) -> Result<Vec<Box<dyn Tool>>> {
        let token = config
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
            .context(
                "GitHub skill requires a token. Add it to your profile:\n\
                 [profiles.<name>.skills.github]\n\
                 token = \"ghp_...\""
            )?;

        let client = Arc::new(GithubClient::new(token)?);

        Ok(vec![
            Box::new(GithubGetRepoTool(client.clone())),
            Box::new(GithubListIssuesTool(client.clone())),
            Box::new(GithubGetIssueTool(client.clone())),
            Box::new(GithubListPrsTool(client.clone())),
            Box::new(GithubGetPrTool(client.clone())),
            Box::new(GithubGetFileTool(client)),
        ])
    }
}
