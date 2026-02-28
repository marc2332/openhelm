# openhelm 🤠

<img src="./logo.png" alt="openhelm logo" width="150" align="right" />

AI-powered bot designed to work on different types of tasks. You talk to it via Telegram. It can act on the host system: reading files, browsing GitHub repos, and more, but limited by a configurable permission and profile system.

> For sake of transparency: This project was 98% done with AI.
> Use at your own responsibility!

---

## Table of Contents

- [How it works](#how-it-works)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Available tools](#available-tools)
- [Docker](#docker)
- [Building from source](#building-from-source)

---

## How it works

```
Telegram user → Bot → Daemon (Unix socket) → LLM + Tools
                          ↑
                    CLI client
```

The daemon runs as a background service. Users pair their Telegram account with the bot; admins approve requests and assign a profile. Every tool call is audit-logged.

---

## Installation

Install openhelm directly from the GitHub repository using Cargo:

```sh
cargo install --git https://github.com/marc2332/openhelm
```

---

## Quick start

**1. Run the interactive setup wizard** — creates `~/openhelm.toml`:

```sh
openhelm setup
```

Or run non-interactively:

```sh
openhelm setup \
  --api-key sk-... \
  --api-url https://openrouter.ai/api/v1 \
  --model gpt-4o \
  --telegram-token 123:ABC \
  --profile default \
  --enable-fs true \
  --fs-read ~/docs \
  --fs-list ~/docs
```

**2. Install as a systemd user service:**

```sh
openhelm install-service
```

**3. Start the daemon:**

```sh
openhelm start
```

---

## Commands

### Daemon

```sh
openhelm start            # Run the daemon in the foreground
openhelm stop             # Shut down the running daemon
openhelm restart          # Restart via systemctl
openhelm status           # Show uptime, sessions, Telegram connection, and paired users
```

### Logs & audit

```sh
openhelm logs             # Show last 50 log lines
openhelm logs -f          # Follow live log output
openhelm logs -n 100      # Show last 100 lines

openhelm audit            # Show last 50 audit entries (JSONL)
openhelm audit -f         # Follow audit log in real time
openhelm audit --user 42  # Filter by Telegram user ID
```

### Chat (local REPL)

Start a local chat session against a profile without Telegram:

```sh
openhelm chat --profile default
```

In-session commands: `/reset` clears history, `exit` or `quit` ends the session.

### User & pairing management

When a Telegram user sends `/start` to the bot, they appear in the pending pair list:

```sh
openhelm pair list                                  # List pending pair requests
openhelm pair approve 123456789 --profile default   # Approve and assign a profile
openhelm pair reject  123456789                     # Reject a request

openhelm users list                                 # List all paired users
openhelm users remove 123456789                     # Remove a user and reset their session
```

### Profiles

```sh
openhelm profiles list    # Show all profiles with permissions and filesystem paths
```

---

## Configuration

The config file lives at `~/openhelm.toml`. Use the provided example as a starting point:

```sh
cp openhelm.example.toml ~/openhelm.toml
```

### Example config

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

[profiles.alice.permissions.skills.http]
# max_body_bytes = 15728640  # optional, defaults to 15 MiB

[audit]
log_path = "~/.local/share/openhelm/audit.log"
```

---

## Available tools

All filesystem paths are allowlist-controlled. Any access outside the configured paths is denied and logged.

| Tool | Skill | Description |
|---|---|---|
| `fs_read` | built-in | Read a file |
| `fs_write` | built-in | Write a file |
| `fs_list` | built-in | List a directory |
| `fs_mkdir` | built-in | Create a directory |
| `github_get_repo` | github | Fetch repository metadata |
| `github_list_issues` | github | List issues |
| `github_get_issue` | github | Get issue details and comments |
| `github_list_prs` | github | List pull requests |
| `github_get_pr` | github | Get PR diff, description, and comments |
| `github_get_file` | github | Read a file from a repository |
| `http_get` | http | Perform an HTTP GET request |
| `http_post` | http | Perform an HTTP POST request with an optional JSON body |
| `http_put` | http | Perform an HTTP PUT request with an optional JSON body |
| `http_patch` | http | Perform an HTTP PATCH request with an optional JSON body |
| `http_delete` | http | Perform an HTTP DELETE request |
| `http_head` | http | Perform an HTTP HEAD request (returns status and headers only) |

---

## Docker

Run openhelm in a container instead of building locally.

### Build the image

```sh
docker build -t openhelm .
```

### Run with Docker Compose (recommended)

1. Copy and edit the config:

```sh
cp openhelm.example.toml openhelm.toml
# Fill in your API keys, bot token, and profiles
```

2. Start the daemon:

```sh
docker compose up -d
```

The `docker-compose.yml` bind-mounts `./openhelm.toml` into the container and persists audit logs in a named volume.

3. Manage the running instance:

```sh
docker compose logs -f
docker compose exec openhelm openhelm status
docker compose exec openhelm openhelm pair list
docker compose exec openhelm openhelm pair approve 123 --profile default
docker compose exec openhelm openhelm users list
```

### Run with Docker directly

```sh
docker run -d \
  --name openhelm \
  --restart unless-stopped \
  -v ./openhelm.toml:/root/openhelm.toml:ro \
  openhelm
```

The config file is expected at `/root/openhelm.toml` inside the container.

---

## Building from source

Requires Rust stable. The workspace contains three crates:

| Crate | Description |
|---|---|
| `openhelm` | Main binary |
| `openhelm-sdk` | Plugin API |
| `openhelm-github` | GitHub skill |
| `openhelm-http` | HTTP skill |

```sh
cargo build --release
```
