mod ai;
mod audit;
mod config;
mod daemon;
mod ipc;
mod permissions;
mod telegram;
mod tools;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ipc::{client_call, IpcRequest, IpcResponse};
use permissions::Permission;
use std::io::{BufRead, Write};
use tokio::io::AsyncBufReadExt;
use tracing::info;
use tracing_subscriber::EnvFilter;

// ─── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "opencontrol",
    about = "AI-powered control service",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground
    Start,
    /// Stop the daemon
    Stop,
    /// Show daemon status
    Status,
    /// Restart the daemon
    Restart,
    /// Tail daemon logs (via journalctl)
    Logs {
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage pairing requests
    #[command(subcommand)]
    Pair(PairCommand),
    /// Manage paired users
    #[command(subcommand)]
    Users(UsersCommand),
    /// View the audit log
    Audit {
        /// Follow audit log output
        #[arg(short, long)]
        follow: bool,
        /// Filter by telegram user ID
        #[arg(long)]
        user: Option<i64>,
        /// Number of lines to show (default: 50)
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
    /// Start an interactive AI chat session via CLI
    Chat,
    /// Generate the default config file
    Init,
    /// Install the systemd user service unit (then use systemctl --user start opencontrol)
    InstallService,
}

#[derive(Subcommand)]
enum PairCommand {
    /// List pending pairing requests
    List,
    /// Approve a pairing request
    Approve {
        telegram_id: i64,
        /// Permissions to grant (comma-separated: fs)
        #[arg(short, long, default_value = "fs")]
        permissions: String,
        /// Allowed filesystem paths (comma-separated)
        #[arg(short = 'a', long, default_value = "")]
        allowed_paths: String,
    },
    /// Reject a pairing request
    Reject { telegram_id: i64 },
}

#[derive(Subcommand)]
enum UsersCommand {
    /// List all paired users
    List,
    /// Remove a paired user
    Remove { telegram_id: i64 },
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialise tracing (respects RUST_LOG env var)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(
            "opencontrol=info".parse().unwrap(),
        ))
        .with_target(false)
        .compact()
        .init();

    match cli.command {
        Command::Init => cmd_init().await,
        Command::Start => cmd_start().await,
        Command::Stop => cmd_stop().await,
        Command::Status => cmd_status().await,
        Command::Restart => cmd_restart().await,
        Command::Logs { follow } => cmd_logs(follow),
        Command::Pair(sub) => match sub {
            PairCommand::List => cmd_pair_list().await,
            PairCommand::Approve {
                telegram_id,
                permissions,
                allowed_paths,
            } => cmd_pair_approve(telegram_id, &permissions, &allowed_paths).await,
            PairCommand::Reject { telegram_id } => cmd_pair_reject(telegram_id).await,
        },
        Command::Users(sub) => match sub {
            UsersCommand::List => cmd_users_list().await,
            UsersCommand::Remove { telegram_id } => cmd_users_remove(telegram_id).await,
        },
        Command::Audit { follow, user, lines } => cmd_audit(follow, user, lines).await,
        Command::Chat => cmd_chat().await,
        Command::InstallService => cmd_install_service().await,
    }
}

// ─── Command implementations ──────────────────────────────────────────────────

async fn cmd_init() -> Result<()> {
    let path = config::Config::path();
    if path.exists() {
        bail!("Config already exists at {}", path.display());
    }
    let cfg = config::Config::default();
    cfg.save().await?;
    println!("Created default config at {}", path.display());
    println!("Edit it to add your API key, Telegram bot token, etc.");
    Ok(())
}

async fn cmd_start() -> Result<()> {
    let cfg = config::Config::load()
        .await
        .context("Failed to load config. Run `opencontrol init` first.")?;
    info!("Starting daemon");
    let d = daemon::Daemon::new(cfg).await?;
    d.run().await
}

async fn cmd_stop() -> Result<()> {
    let cfg = config::Config::load().await.ok();
    let socket = cfg
        .as_ref()
        .map(|c| c.daemon.socket_path.as_str())
        .unwrap_or("/tmp/opencontrol.sock");

    match client_call(socket, &IpcRequest::Shutdown).await {
        Ok(_) => println!("Daemon stopped"),
        Err(e) => bail!("Daemon is not running: {}", e),
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let cfg = config::Config::load().await.ok();
    let socket = cfg
        .as_ref()
        .map(|c| c.daemon.socket_path.as_str())
        .unwrap_or("/tmp/opencontrol.sock");

    match client_call(socket, &IpcRequest::Status).await {
        Ok(IpcResponse::Status {
            uptime_seconds,
            active_sessions,
            paired_users,
            pending_pairs,
            telegram_connected,
        }) => {
            let uptime = format_uptime(uptime_seconds);
            println!("Daemon:           running");
            println!("Uptime:           {}", uptime);
            println!("Active sessions:  {}", active_sessions);
            println!("Paired users:     {}", paired_users);
            println!("Pending pairs:    {}", pending_pairs);
            println!(
                "Telegram:         {}",
                if telegram_connected {
                    "connected"
                } else {
                    "not configured"
                }
            );
        }
        Ok(_) => bail!("Unexpected response from daemon"),
        Err(e) => {
            println!("Daemon:           not running");
            println!("  ({})", e);
        }
    }
    Ok(())
}

async fn cmd_restart() -> Result<()> {
    // If managed by systemd, delegate to it; otherwise not supported
    // (restarting a foreground process from a sibling CLI process doesn't make sense)
    let result = std::process::Command::new("systemctl")
        .args(["--user", "restart", "opencontrol"])
        .status();
    match result {
        Ok(s) if s.success() => println!("opencontrol restarted (systemd)"),
        _ => bail!(
            "Restart is only supported when running as a systemd service.\n\
            Install with `opencontrol install-service`, or stop and re-run `opencontrol start` manually."
        ),
    }
    Ok(())
}

fn cmd_logs(follow: bool) -> Result<()> {
    let mut args = vec!["--user", "-u", "opencontrol.service"];
    if follow {
        args.push("-f");
    }
    let status = std::process::Command::new("journalctl")
        .args(&args)
        .status()
        .context("Failed to run journalctl")?;
    if !status.success() {
        bail!("journalctl exited with error");
    }
    Ok(())
}

async fn cmd_pair_list() -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = &cfg.daemon.socket_path;

    match client_call(socket, &IpcRequest::PairList).await? {
        IpcResponse::PairList { pending } => {
            if pending.is_empty() {
                println!("No pending pairing requests");
            } else {
                println!(
                    "{:<15} {:<20} {}",
                    "Telegram ID", "Username", "Requested At"
                );
                println!("{}", "-".repeat(60));
                for p in &pending {
                    println!(
                        "{:<15} {:<20} {}",
                        p.telegram_id, p.username, p.requested_at
                    );
                }
                println!("\nApprove:  opencontrol pair approve <telegram_id>");
                println!("Reject:   opencontrol pair reject <telegram_id>");
            }
        }
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_pair_approve(
    telegram_id: i64,
    permissions_str: &str,
    paths_str: &str,
) -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = &cfg.daemon.socket_path;

    let permissions: Vec<Permission> = permissions_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "fs" => Ok(Permission::Fs),
            other => bail!("Unknown permission: '{}'. Valid: fs", other),
        })
        .collect::<Result<_>>()?;

    let fs_allowed_paths: Vec<String> = paths_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if permissions.contains(&Permission::Fs) && fs_allowed_paths.is_empty() {
        println!(
            "Warning: fs permission granted but no --allowed-paths specified. \
            The user will get a clear error when the AI tries to access any path. \
            Re-approve with: opencontrol pair approve {} --allowed-paths /some/path",
            telegram_id
        );
    }

    let req = IpcRequest::PairApprove {
        telegram_id,
        permissions,
        fs_allowed_paths,
    };

    match client_call(socket, &req).await? {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_pair_reject(telegram_id: i64) -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = &cfg.daemon.socket_path;

    match client_call(socket, &IpcRequest::PairReject { telegram_id }).await? {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_users_list() -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = &cfg.daemon.socket_path;

    match client_call(socket, &IpcRequest::UsersList).await? {
        IpcResponse::UsersList { users } => {
            if users.is_empty() {
                println!("No paired users");
            } else {
                println!(
                    "{:<15} {:<20} {:<15} {}",
                    "Telegram ID", "Name", "Permissions", "Allowed Paths"
                );
                println!("{}", "-".repeat(80));
                for u in &users {
                    let perms = u
                        .permissions
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let paths = u.fs_allowed_paths.join(", ");
                    println!(
                        "{:<15} {:<20} {:<15} {}",
                        u.telegram_id, u.name, perms, paths
                    );
                }
            }
        }
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_users_remove(telegram_id: i64) -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = &cfg.daemon.socket_path;

    match client_call(socket, &IpcRequest::UserRemove { telegram_id }).await? {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_audit(follow: bool, filter_user: Option<i64>, lines: usize) -> Result<()> {
    let cfg = config::Config::load().await?;
    let log_path = &cfg.audit.log_path;

    if follow {
        use tokio::fs::File;
        use tokio::io::BufReader;

        let file = File::open(log_path)
            .await
            .with_context(|| format!("Cannot open audit log at {}", log_path))?;

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        // Drain existing content to seek to end
        while reader.read_line(&mut line).await? > 0 {
            line.clear();
        }

        println!("Following audit log (Ctrl+C to stop)...");
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            print_audit_line(line.trim(), filter_user);
        }
    } else {
        let contents = tokio::fs::read_to_string(log_path)
            .await
            .with_context(|| format!("Cannot open audit log at {}", log_path))?;

        let all_lines: Vec<&str> = contents.lines().collect();
        let start = all_lines.len().saturating_sub(lines);
        for line in &all_lines[start..] {
            print_audit_line(line, filter_user);
        }
    }

    Ok(())
}

fn print_audit_line(line: &str, filter_user: Option<i64>) {
    if let Some(uid) = filter_user {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let line_uid = val.get("user_id").and_then(|v| v.as_i64());
            if line_uid != Some(uid) {
                return;
            }
        }
    }
    println!("{}", line);
}

async fn cmd_chat() -> Result<()> {
    let cfg = config::Config::load().await?;
    let socket = cfg.daemon.socket_path.clone();

    println!("OpenControl CLI Chat (type 'exit' or Ctrl+C to quit, '/reset' to clear history)");
    println!("{}", "-".repeat(60));

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    loop {
        print!("> ");
        stdout.flush()?;

        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let input = line.trim();

        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }
        if input == "/reset" {
            match client_call(&socket, &IpcRequest::ChatReset).await? {
                IpcResponse::Ok { message } => println!("  [{}]", message),
                _ => {}
            }
            continue;
        }

        match client_call(
            &socket,
            &IpcRequest::Chat {
                message: input.to_string(),
            },
        )
        .await?
        {
            IpcResponse::ChatReply { message } => {
                println!("\nAssistant: {}\n", message);
            }
            IpcResponse::Error { message } => {
                eprintln!("Error: {}", message);
            }
            _ => eprintln!("Unexpected response"),
        }
    }

    Ok(())
}

async fn cmd_install_service() -> Result<()> {
    let binary = std::env::current_exe().context("Cannot determine binary path")?;
    let binary_str = binary.to_string_lossy();

    let service = format!(
        "[Unit]\nDescription=OpenControl AI daemon\nAfter=network.target\n\n\
        [Service]\nType=simple\nExecStart={binary} start\n\
        Restart=on-failure\nRestartSec=5\n\n\
        [Install]\nWantedBy=default.target\n",
        binary = binary_str
    );

    let systemd_dir = {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        std::path::PathBuf::from(home)
            .join(".config")
            .join("systemd")
            .join("user")
    };
    tokio::fs::create_dir_all(&systemd_dir).await?;

    let unit_path = systemd_dir.join("opencontrol.service");
    tokio::fs::write(&unit_path, &service).await?;
    println!("Wrote systemd unit to {}", unit_path.display());

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "opencontrol"])
        .status()
        .context("Failed to run systemctl")?;

    if status.success() {
        println!("Service enabled.");
    } else {
        println!("Warning: systemctl enable failed.");
    }
    println!();
    println!("To manage the service:");
    println!("  systemctl --user start opencontrol");
    println!("  systemctl --user stop opencontrol");
    println!("  systemctl --user status opencontrol");
    println!("  journalctl --user -u opencontrol.service -f");

    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let mins = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}
