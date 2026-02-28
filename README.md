# openhelm

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
# Interactive wizard — creates ~/openhelm.toml
openhelm setup

# Or non-interactive
openhelm setup \
  --api-key sk-... \
  --api-url https://openrouter.ai/api/v1 \
  --model gpt-4o \
  --telegram-token 123:ABC \
  --profile default \
  --enable-fs true \
  --fs-read ~/docs \
  --fs-list ~/docs

# Install as a systemd user service
openhelm install-service

# Start manually (foreground)
openhelm start
```

## Commands

### Daemon

```sh
openhelm start            # Run the daemon in the foreground
openhelm stop             # Shut down the running daemon
openhelm restart          # Restart via systemctl
openhelm status           # Uptime, sessions, Telegram connection, paired users
```

### Logs & Audit

```sh
openhelm logs             # Last 50 log lines
openhelm logs -f          # Follow live log output
openhelm logs -n 100      # Show last 100 lines

openhelm audit            # Last 50 audit entries (JSONL)
openhelm audit -f         # Follow audit log in real time
openhelm audit --user 42  # Filter by Telegram user ID
```

### Chat (local REPL)

```sh
openhelm chat --profile default
```

In-session commands: `/reset` clears history, `exit` or `quit` ends the session.

### User & Pairing Management

```sh
# When a Telegram user sends /start to the bot, they appear here:
openhelm pair list

# Approve and assign a profile
openhelm pair approve 123456789 --profile default

# Reject a request
openhelm pair reject 123456789

# List all paired users
openhelm users list

# Remove a user and reset their session
openhelm users remove 123456789
```

### Profiles

```sh
openhelm profiles list    # Show all profiles with permissions and FS paths
```

## Configuration

`~/openhelm.toml` — copy from `openhelm.example.toml`.

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
log_path = "~/.local/share/openhelm/audit.log"
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

## Docker

You can run openhelm in a Docker container instead of installing Rust and building locally.

### Build the image

```sh
docker build -t openhelm .
```

### Run with Docker Compose (recommended)

1. Create your config file:

```sh
cp openhelm.example.toml openhelm.toml
# Edit openhelm.toml with your API keys, bot token, profiles, etc.
```

2. Start the daemon:

```sh
docker compose up -d
```

The `docker-compose.yml` bind-mounts `./openhelm.toml` into the container and persists audit logs in a named volume.

3. Manage the running instance:

```sh
docker compose logs -f                                           # follow daemon logs
docker compose exec openhelm openhelm status               # check daemon status
docker compose exec openhelm openhelm pair list            # list pending pairs
docker compose exec openhelm openhelm pair approve 123 --profile default
docker compose exec openhelm openhelm users list           # list paired users
```

### Run with Docker directly

```sh
docker run -d \
  --name openhelm \
  --restart unless-stopped \
  -v ./openhelm.toml:/root/openhelm.toml:ro \
  openhelm
```

The config file is expected at `/root/openhelm.toml` inside the container. Mount your local config there with `-v`.

## Building

```sh
cargo build --release
```

Requires Rust stable. The workspace has three crates: `openhelm` (binary), `openhelm-sdk` (plugin API), `openhelm-github` (GitHub skill).
