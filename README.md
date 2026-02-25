# opencontrol

An AI-powered daemon that bridges a Telegram bot with any OpenAI-compatible LLM. Authorized users chat with an AI assistant that can act on the host system — reading files, browsing GitHub repos, and more — through a configurable permission system.

## How it works

```
Telegram user → Bot → Daemon (Unix socket) → LLM + Tools
                          ↑
                    CLI client
```

The daemon runs as a background service. Users pair their Telegram account; admins approve and assign a profile. Every tool call is audit-logged.

## Setup

```sh
# Interactive wizard — creates ~/opencontrol.toml
opencontrol setup

# Or non-interactive
opencontrol setup \
  --api-key sk-... \
  --api-url https://openrouter.ai/api/v1 \
  --model gpt-4o \
  --telegram-token 123:ABC \
  --profile default \
  --enable-fs true \
  --fs-read ~/docs \
  --fs-list ~/docs

# Install as a systemd user service
opencontrol install-service

# Start manually (foreground)
opencontrol start
```

## Commands

### Daemon

```sh
opencontrol start            # Run the daemon in the foreground
opencontrol stop             # Shut down the running daemon
opencontrol restart          # Restart via systemctl
opencontrol status           # Uptime, sessions, Telegram connection, paired users
```

### Logs & Audit

```sh
opencontrol logs             # Last 50 log lines
opencontrol logs -f          # Follow live log output
opencontrol logs -n 100      # Show last 100 lines

opencontrol audit            # Last 50 audit entries (JSONL)
opencontrol audit -f         # Follow audit log in real time
opencontrol audit --user 42  # Filter by Telegram user ID
```

### Chat (local REPL)

```sh
opencontrol chat --profile default
```

In-session commands: `/reset` clears history, `exit` or `quit` ends the session.

### User & Pairing Management

```sh
# When a Telegram user sends /start to the bot, they appear here:
opencontrol pair list

# Approve and assign a profile
opencontrol pair approve 123456789 --profile default

# Reject a request
opencontrol pair reject 123456789

# List all paired users
opencontrol users list

# Remove a user and reset their session
opencontrol users remove 123456789
```

### Profiles

```sh
opencontrol profiles list    # Show all profiles with permissions and FS paths
```

## Configuration

`~/opencontrol.toml` — copy from `opencontrol.example.toml`.

```toml
[ai]
api_url = "https://openrouter.ai/api/v1"
api_key = "sk-..."
model   = "gpt-4o"
session_timeout_minutes = 30

[telegram]
bot_token = "123:ABC..."

[profiles.alice]
system_prompt = "You are a coding assistant."

[profiles.alice.permissions]
fs = true

[profiles.alice.fs]
read     = ["/home/alice/projects"]
read_dir = ["/home/alice/projects"]
write    = ["/home/alice/projects"]
mkdir    = []

[profiles.alice.permissions.skills.github]
token = "ghp_..."

[audit]
log_path = "~/.local/share/opencontrol/audit.log"
```

## Tools

| Tool | Skill | Description |
|------|-------|-------------|
| `fs_read` | built-in | Read a file |
| `fs_write` | built-in | Write a file |
| `fs_list` | built-in | List a directory |
| `fs_mkdir` | built-in | Create a directory |
| `github_get_repo` | github | Repo metadata |
| `github_list_issues` | github | List issues |
| `github_get_issue` | github | Issue details + comments |
| `github_list_prs` | github | List pull requests |
| `github_get_pr` | github | PR diff, description, comments |
| `github_get_file` | github | Read a file from a repo |

All filesystem paths are allowlist-controlled. Access outside configured paths is denied and logged.

## Building

```sh
cargo build --release
```

Requires Rust stable. The workspace has three crates: `opencontrol` (binary), `opencontrol-sdk` (plugin API), `opencontrol-github` (GitHub skill).
