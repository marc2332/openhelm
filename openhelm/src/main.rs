mod ai;
mod audit;
mod config;
mod daemon;
mod ipc;
mod log_buffer;
mod permissions;
mod telegram;
mod tools;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ipc::{IpcRequest, IpcResponse, client_call, client_stream, recv_response_from};
use log_buffer::LogBuffer;
use rustls::crypto::ring::default_provider;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tracing::info;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "openhelm", about = "AI-powered control service", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Start,
    Stop,
    Status,
    Restart,
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
    #[command(subcommand)]
    Pair(PairCommand),
    #[command(subcommand)]
    Users(UsersCommand),
    #[command(subcommand)]
    Profiles(ProfilesCommand),
    Audit {
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        user: Option<i64>,
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
    Chat {
        #[arg(short, long)]
        profile: String,
    },
    Setup {
        #[arg(long)]
        api_url: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        telegram_token: Option<String>,
        #[arg(long)]
        github_token: Option<String>,
        #[arg(long)]
        enable_fs: Option<bool>,
        #[arg(long)]
        fs_read: Option<Vec<String>>,
        #[arg(long)]
        fs_write: Option<Vec<String>>,
        #[arg(long)]
        fs_list: Option<Vec<String>>,
        #[arg(long)]
        fs_mkdir: Option<Vec<String>>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        force: bool,
    },
    InstallService,
}

#[derive(Subcommand)]
enum PairCommand {
    List,
    Approve {
        telegram_id: i64,
        #[arg(short, long)]
        profile: String,
    },
    Reject {
        telegram_id: i64,
    },
}

#[derive(Subcommand)]
enum UsersCommand {
    List,
    Remove { telegram_id: i64 },
}

#[derive(Subcommand)]
enum ProfilesCommand {
    List,
}

struct LogBufferWriter(Arc<LogBuffer>);

impl std::io::Write for LogBufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(text) = std::str::from_utf8(buf) {
            let line = text.trim_end_matches('\n').to_string();
            if !line.is_empty() {
                self.0.push(line);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct MakeLogBufferWriter(Arc<LogBuffer>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeLogBufferWriter {
    type Writer = LogBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogBufferWriter(self.0.clone())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let cli = Cli::parse();
    let log_buf = Arc::new(LogBuffer::new(1000));

    let env_filter = EnvFilter::from_default_env().add_directive("openhelm=info".parse().unwrap());

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact()
                .with_filter(env_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(MakeLogBufferWriter(log_buf.clone()))
                .with_filter(EnvFilter::new("openhelm=info")),
        )
        .init();

    match cli.command {
        Command::Setup {
            api_url,
            api_key,
            model,
            telegram_token,
            github_token,
            enable_fs,
            fs_read,
            fs_write,
            fs_list,
            fs_mkdir,
            profile,
            force,
        } => {
            cmd_setup(
                api_url,
                api_key,
                model,
                telegram_token,
                github_token,
                enable_fs,
                fs_read,
                fs_write,
                fs_list,
                fs_mkdir,
                profile,
                force,
            )
            .await
        }
        Command::Start => cmd_start(log_buf).await,
        Command::Stop => cmd_stop().await,
        Command::Status => cmd_status().await,
        Command::Restart => cmd_restart().await,
        Command::Logs { follow, lines } => cmd_logs(follow, lines).await,
        Command::Pair(sub) => match sub {
            PairCommand::List => cmd_pair_list().await,
            PairCommand::Approve {
                telegram_id,
                profile,
            } => cmd_pair_approve(telegram_id, profile).await,
            PairCommand::Reject { telegram_id } => cmd_pair_reject(telegram_id).await,
        },
        Command::Users(sub) => match sub {
            UsersCommand::List => cmd_users_list().await,
            UsersCommand::Remove { telegram_id } => cmd_users_remove(telegram_id).await,
        },
        Command::Profiles(sub) => match sub {
            ProfilesCommand::List => cmd_profiles_list().await,
        },
        Command::Audit {
            follow,
            user,
            lines,
        } => cmd_audit(follow, user, lines).await,
        Command::Chat { profile } => cmd_chat(profile).await,
        Command::InstallService => cmd_install_service().await,
    }
}

async fn cmd_setup(
    api_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    telegram_token: Option<String>,
    github_token: Option<String>,
    enable_fs: Option<bool>,
    fs_read: Option<Vec<String>>,
    fs_write: Option<Vec<String>>,
    fs_list: Option<Vec<String>>,
    fs_mkdir: Option<Vec<String>>,
    profile: Option<String>,
    force: bool,
) -> Result<()> {
    use dialoguer::Input;
    use std::collections::HashMap;

    let path = config::Config::path();

    let has_cli_args = api_key.is_some();

    if path.exists() && !force {
        if has_cli_args {
            bail!("Config already exists. Use --force to overwrite.");
        }
        let overwrite: bool = Input::new()
            .with_prompt("Config already exists. Overwrite?")
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Aborted.");
            return Ok(());
        }
    }

    let api_url: Option<String> = if api_url.is_some() {
        api_url
    } else if has_cli_args {
        // CLI mode without explicit --api-url → leave as None (auto-detect)
        None
    } else {
        let input: String = Input::new()
            .with_prompt("API URL (leave empty to auto-detect from key)")
            .default(String::new())
            .interact()
            .unwrap();
        if input.is_empty() { None } else { Some(input) }
    };

    let api_key = if let Some(key) = api_key {
        key
    } else {
        let input: String = Input::new().with_prompt("API Key (required)").interact()?;
        if input.is_empty() {
            bail!("API key is required");
        }
        input
    };

    let model = model.unwrap_or_else(|| {
        if has_cli_args {
            "gpt-4o".to_string()
        } else {
            let input: String = Input::new()
                .with_prompt("Model")
                .default("gpt-4o".to_string())
                .interact()
                .unwrap();
            input
        }
    });

    let telegram_token = telegram_token.unwrap_or_else(|| {
        if has_cli_args {
            String::new()
        } else {
            let input: String = Input::new()
                .with_prompt("Bot token (optional, press Enter to skip)")
                .default(String::new())
                .interact()
                .unwrap();
            input
        }
    });

    let github_token = github_token.unwrap_or_else(|| {
        if has_cli_args {
            String::new()
        } else {
            let input: String = Input::new()
                .with_prompt("GitHub token (optional, press Enter to skip)")
                .default(String::new())
                .interact()
                .unwrap();
            input
        }
    });

    let enable_fs = enable_fs.unwrap_or_else(|| {
        if has_cli_args {
            false
        } else {
            let input: bool = Input::new()
                .with_prompt("Enable filesystem tools?")
                .default(false)
                .interact()
                .unwrap();
            input
        }
    });

    let fs_permissions = if enable_fs {
        let expand_tilde = |s: String| -> String {
            if s.starts_with("~/") {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                format!("{}{}", home, &s[1..])
            } else {
                s
            }
        };

        let read = fs_read
            .map(|paths| paths.into_iter().map(expand_tilde).collect())
            .unwrap_or_else(|| {
                if has_cli_args {
                    vec![]
                } else {
                    let input: String = Input::new()
                        .with_prompt("Read paths (comma-separated, ~/ expands)")
                        .default(String::new())
                        .interact()
                        .unwrap();
                    input
                        .split(',')
                        .map(|part| part.trim().to_string())
                        .filter(|part| !part.is_empty())
                        .map(expand_tilde)
                        .collect()
                }
            });

        let write = fs_write
            .map(|paths| paths.into_iter().map(expand_tilde).collect())
            .unwrap_or_else(|| {
                if has_cli_args {
                    vec![]
                } else {
                    let input: String = Input::new()
                        .with_prompt("Write paths (comma-separated)")
                        .default(String::new())
                        .interact()
                        .unwrap();
                    input
                        .split(',')
                        .map(|part| part.trim().to_string())
                        .filter(|part| !part.is_empty())
                        .map(expand_tilde)
                        .collect()
                }
            });

        let read_dir = fs_list
            .map(|paths| paths.into_iter().map(expand_tilde).collect())
            .unwrap_or_else(|| {
                if has_cli_args {
                    vec![]
                } else {
                    let input: String = Input::new()
                        .with_prompt("List paths (comma-separated)")
                        .default(String::new())
                        .interact()
                        .unwrap();
                    input
                        .split(',')
                        .map(|part| part.trim().to_string())
                        .filter(|part| !part.is_empty())
                        .map(expand_tilde)
                        .collect()
                }
            });

        let mkdir = fs_mkdir
            .map(|paths| paths.into_iter().map(expand_tilde).collect())
            .unwrap_or_else(|| {
                if has_cli_args {
                    vec![]
                } else {
                    let input: String = Input::new()
                        .with_prompt("Mkdir paths (comma-separated)")
                        .default(String::new())
                        .interact()
                        .unwrap();
                    input
                        .split(',')
                        .map(|part| part.trim().to_string())
                        .filter(|part| !part.is_empty())
                        .map(expand_tilde)
                        .collect()
                }
            });

        config::FsPermissions {
            read,
            write,
            read_dir,
            mkdir,
        }
    } else {
        config::FsPermissions::default()
    };

    let profile_name = profile.unwrap_or_else(|| {
        if has_cli_args {
            "default".to_string()
        } else {
            let input: String = Input::new()
                .with_prompt("Profile name")
                .default("default".to_string())
                .interact()
                .unwrap();
            input
        }
    });

    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());

    let mut profile = config::Profile::default();
    profile.permissions.fs = enable_fs;
    if enable_fs {
        profile.fs = Some(fs_permissions);
    }

    let mut skills = HashMap::new();
    if !github_token.is_empty() {
        let mut github_config = toml::map::Map::new();
        github_config.insert("token".to_string(), toml::Value::String(github_token));
        skills.insert("github".to_string(), toml::Value::Table(github_config));
    }
    profile.permissions.skills = skills;

    let mut profiles = HashMap::new();
    profiles.insert(profile_name, profile);

    let cfg = config::Config {
        daemon: config::DaemonConfig {
            socket_path: "/tmp/openhelm.sock".to_string(),
            log_level: "info".to_string(),
        },
        ai: config::AiConfig {
            api_url,
            api_key,
            model,
            system_prompt: "You are a helpful assistant with access to tools on the host system. Use them carefully and only when necessary.".to_string(),
            session_timeout_minutes: 30,
        },
        telegram: config::TelegramConfig {
            bot_token: telegram_token,
            users: vec![],
        },
        audit: config::AuditConfig {
            log_path: format!("{}/.local/share/openhelm/audit.log", home),
        },
        profiles,
    };

    cfg.save().await?;
    println!("\n✅ Saved config to {}", path.display());
    Ok(())
}

async fn cmd_start(log_buf: Arc<LogBuffer>) -> Result<()> {
    let cfg = config::Config::load()
        .await
        .context("Failed to load config. Run `openhelm setup` first.")?;
    info!("Starting daemon");
    let daemon = daemon::Daemon::new(cfg, log_buf).await?;
    daemon.run().await
}

async fn cmd_stop() -> Result<()> {
    let cfg = config::Config::load().await.ok();
    match client_call(socket_path(cfg.as_ref()), &IpcRequest::Shutdown).await {
        Ok(_) => println!("Daemon stopped"),
        Err(err) => bail!("Daemon is not running: {}", err),
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let cfg = config::Config::load().await.ok();
    match client_call(socket_path(cfg.as_ref()), &IpcRequest::Status).await {
        Ok(IpcResponse::Status {
            uptime_seconds,
            active_sessions,
            paired_users,
            pending_pairs,
            telegram_connected,
        }) => {
            println!("Daemon:           running");
            println!("Uptime:           {}", format_uptime(uptime_seconds));
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
        Err(err) => {
            println!("Daemon:           not running");
            println!("  ({})", err);
        }
    }
    Ok(())
}

async fn cmd_restart() -> Result<()> {
    let result = std::process::Command::new("systemctl")
        .args(["--user", "restart", "openhelm"])
        .status();
    match result {
        Ok(status) if status.success() => println!("openhelm restarted (systemd)"),
        _ => bail!(
            "Restart is only supported when running as a systemd service.\n\
            Install with `openhelm install-service`, or stop and re-run `openhelm start`."
        ),
    }
    Ok(())
}

async fn cmd_logs(follow: bool, lines: usize) -> Result<()> {
    let cfg = config::Config::load().await.ok();
    let socket = socket_path(cfg.as_ref());
    let total = match client_call(socket, &IpcRequest::Logs { lines, offset: 0 }).await {
        Ok(IpcResponse::Logs {
            lines: log_lines,
            total,
        }) => {
            for line in &log_lines {
                println!("{}", line);
            }
            total
        }
        Err(err) => {
            eprintln!("Daemon is not running: {}", err);
            eprintln!(
                "Hint: if running under systemd try: journalctl --user -u openhelm.service -f"
            );
            return Ok(());
        }
        Ok(_) => bail!("Unexpected response from daemon"),
    };

    if follow {
        let mut offset = total;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match client_call(socket, &IpcRequest::Logs { lines: 0, offset }).await {
                Ok(IpcResponse::Logs {
                    lines: new_lines,
                    total: new_total,
                }) => {
                    for line in &new_lines {
                        println!("{}", line);
                    }
                    offset = new_total;
                }
                Err(_) => {
                    eprintln!("--- daemon disconnected ---");
                    return Ok(());
                }
                Ok(_) => {}
            }
        }
    }

    Ok(())
}

async fn cmd_pair_list() -> Result<()> {
    let cfg = config::Config::load().await?;
    match client_call(&cfg.daemon.socket_path, &IpcRequest::PairList).await? {
        IpcResponse::PairList { pending } => {
            if pending.is_empty() {
                println!("No pending pairing requests");
            } else {
                println!(
                    "{:<15} {:<20} {}",
                    "Telegram ID", "Username", "Requested At"
                );
                println!("{}", "-".repeat(60));
                for pair in &pending {
                    println!(
                        "{:<15} {:<20} {}",
                        pair.telegram_id, pair.username, pair.requested_at
                    );
                }
                println!("\nApprove:  openhelm pair approve <telegram_id> --profile <name>");
                println!("Reject:   openhelm pair reject <telegram_id>");
            }
        }
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_pair_approve(telegram_id: i64, profile: String) -> Result<()> {
    let cfg = config::Config::load().await?;
    cfg.require_profile(&profile)?;

    match client_call(
        &cfg.daemon.socket_path,
        &IpcRequest::PairApprove {
            telegram_id,
            profile,
        },
    )
    .await?
    {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_pair_reject(telegram_id: i64) -> Result<()> {
    let cfg = config::Config::load().await?;
    match client_call(
        &cfg.daemon.socket_path,
        &IpcRequest::PairReject { telegram_id },
    )
    .await?
    {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_users_list() -> Result<()> {
    let cfg = config::Config::load().await?;
    match client_call(&cfg.daemon.socket_path, &IpcRequest::UsersList).await? {
        IpcResponse::UsersList { users } => {
            if users.is_empty() {
                println!("No paired users");
            } else {
                println!("{:<15} {:<20} {}", "Telegram ID", "Name", "Profile");
                println!("{}", "-".repeat(50));
                for user in &users {
                    println!(
                        "{:<15} {:<20} {}",
                        user.telegram_id, user.name, user.profile
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
    match client_call(
        &cfg.daemon.socket_path,
        &IpcRequest::UserRemove { telegram_id },
    )
    .await?
    {
        IpcResponse::Ok { message } => println!("{}", message),
        IpcResponse::Error { message } => bail!("{}", message),
        _ => bail!("Unexpected response"),
    }
    Ok(())
}

async fn cmd_profiles_list() -> Result<()> {
    let cfg = config::Config::load().await?;
    match client_call(&cfg.daemon.socket_path, &IpcRequest::ProfilesList).await? {
        IpcResponse::ProfilesList { mut profiles } => {
            if profiles.is_empty() {
                println!("No profiles configured.");
                println!("Add a [profiles.<name>] section to openhelm.toml.");
                return Ok(());
            }
            profiles.sort_by(|left, right| left.name.cmp(&right.name));
            for profile in &profiles {
                println!("profile: {}", profile.name);
                if let Some(model) = &profile.model {
                    println!("  model:         {}", model);
                }
                println!("  custom prompt: {}", profile.has_custom_prompt);
                println!("  permissions:");
                println!("    fs: {}", profile.fs_enabled);
                if let Some(fs) = profile.fs.as_ref().filter(|_| profile.fs_enabled) {
                    let fmt_paths = |paths: &Vec<String>| {
                        if paths.is_empty() {
                            "(none)".to_string()
                        } else {
                            paths.join(", ")
                        }
                    };
                    println!("      read:     {}", fmt_paths(&fs.read));
                    println!("      read_dir: {}", fmt_paths(&fs.read_dir));
                    println!("      write:    {}", fmt_paths(&fs.write));
                    println!("      mkdir:    {}", fmt_paths(&fs.mkdir));
                } else if profile.fs_enabled {
                    println!("      (no [fs] table - all paths denied)");
                }
                println!();
            }
        }
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
    let dominated = filter_user.is_some_and(|uid| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|val| val.get("user_id").and_then(|id| id.as_i64()))
            != Some(uid)
    });
    if !dominated {
        println!("{}", line);
    }
}

async fn cmd_chat(profile: String) -> Result<()> {
    let cfg = config::Config::load().await?;
    cfg.require_profile(&profile)?;

    let socket = &cfg.daemon.socket_path;

    println!("openhelm CLI Chat [profile: {}]", profile);
    println!("(type 'exit' or Ctrl+C to quit, '/reset' to clear history)");
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
            match client_call(
                &socket,
                &IpcRequest::ChatReset {
                    profile: profile.clone(),
                },
            )
            .await?
            {
                IpcResponse::Ok { message } => println!("  [{}]", message),
                IpcResponse::Error { message } => eprintln!("Error: {}", message),
                _ => eprintln!("Unexpected response"),
            }
            continue;
        }

        let mut reader = client_stream(
            &socket,
            &IpcRequest::Chat {
                message: input.to_string(),
                profile: profile.clone(),
            },
        )
        .await?;

        print!("\nAssistant: ");
        stdout.flush()?;

        let mut got_content = false;
        loop {
            match recv_response_from(&mut reader).await? {
                Some(IpcResponse::ChatChunk { text }) => {
                    got_content = true;
                    print!("{}", text);
                    stdout.flush()?;
                }
                Some(IpcResponse::ChatDone) => {
                    if got_content {
                        println!("\n");
                    } else {
                        println!("(no response)\n");
                    }
                    break;
                }
                Some(IpcResponse::Error { message }) => {
                    if got_content {
                        println!();
                    }
                    eprintln!("Error: {}\n", message);
                    break;
                }
                Some(_) => {
                    eprintln!("Unexpected response\n");
                    break;
                }
                None => {
                    // Connection closed without ChatDone
                    if got_content {
                        println!("\n");
                    }
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn cmd_install_service() -> Result<()> {
    let binary = std::env::current_exe().context("Cannot determine binary path")?;
    let service = format!(
        "[Unit]\nDescription=openhelm AI daemon\nAfter=network.target\n\n\
        [Service]\nType=simple\nExecStart={binary} start\n\
        Restart=on-failure\nRestartSec=5\n\n\
        [Install]\nWantedBy=default.target\n",
        binary = binary.display()
    );

    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let systemd_dir = std::path::PathBuf::from(home).join(".config/systemd/user");
    tokio::fs::create_dir_all(&systemd_dir).await?;

    let unit_path = systemd_dir.join("openhelm.service");
    tokio::fs::write(&unit_path, &service).await?;
    println!("Wrote systemd unit to {}", unit_path.display());

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "openhelm"])
        .status()
        .context("Failed to run systemctl")?;

    if status.success() {
        println!("Service enabled.");
    } else {
        println!("Warning: systemctl enable failed.");
    }
    println!();
    println!("To manage the service:");
    println!("  systemctl --user start openhelm");
    println!("  systemctl --user stop openhelm");
    println!("  systemctl --user status openhelm");
    println!("  journalctl --user -u openhelm.service -f");
    Ok(())
}

fn socket_path(cfg: Option<&config::Config>) -> &str {
    cfg.map(|config| config.daemon.socket_path.as_str())
        .unwrap_or("/tmp/openhelm.sock")
}

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
