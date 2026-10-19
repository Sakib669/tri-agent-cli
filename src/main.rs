mod agent;
mod auth;
mod config;
mod providers;
mod skills;
mod sse;
mod store;
mod theme;
mod tui;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::providers::ProviderId;
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "triagent",
    version,
    about = "CLI agent using your Claude / ChatGPT / Gemini subscriptions (browser OAuth). No daemons, no autostart."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// One-shot prompt (print mode, no interactive UI)
    #[arg(short = 'p', long = "print")]
    prompt: Option<String>,

    /// Provider to use (claude | chatgpt | gemini); overrides config default
    #[arg(long, global = true)]
    provider: Option<ProviderId>,

    /// Model to use; overrides the provider default
    #[arg(long, global = true)]
    model: Option<String>,

    /// Skip all permission prompts (dangerous)
    #[arg(long, global = true)]
    dangerously_skip_permissions: bool,

    /// Resume a previous session by its id
    #[arg(long, global = true)]
    resume: Option<String>,

    /// Extra directories the agent may read/write
    #[arg(long, global = true)]
    add_dir: Vec<PathBuf>,

    /// Verbose logging (secrets are redacted)
    #[arg(long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Browser OAuth login for one provider (uses your subscription, no API key)
    Login { provider: ProviderId },
    /// Remove stored credentials (and revoke remote tokens where supported)
    Logout { provider: ProviderId },
    /// Show login state, plan tier, model and quota info per provider
    Status,
    /// List models available on a provider's subscription
    Models {
        /// Provider (defaults to the configured default)
        provider: Option<ProviderId>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Panic diagnostics to a file (helps debug TUI thread panics).
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(
            "/tmp/triagent-panic.log",
            format!("PANIC at {loc}: {msg}\n{backtrace}"),
        );
        // Re-print to stderr for normal visibility.
        eprintln!("PANIC at {loc}: {msg}");
    }));

    let cli = Cli::parse();
    let config = Config::load_or_default()?;
    let store = Store::open()?;

    // Terminal close (SIGHUP) / SIGTERM: kill any running tool process groups
    // and exit. Nothing daemonizes, so this is the only cleanup path needed.
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            agent::tools::kill_all_children();
            // Best-effort terminal restoration before exiting.
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableBracketedPaste
            );
            std::process::exit(143);
        });
    }

    match cli.command {
        Some(Cmd::Login { provider }) => {
            auth::login(&provider, &store).await?;
        }
        Some(Cmd::Logout { provider }) => {
            auth::logout(&provider, &store).await?;
        }
        Some(Cmd::Status) => {
            auth::status(&config, &store).await?;
        }
        Some(Cmd::Models { provider }) => {
            let pid = provider
                .or(cli.provider)
                .unwrap_or(config.default_provider());
            auth::list_models(&pid, &store).await?;
        }
        None => {
            let provider = cli.provider.unwrap_or(config.default_provider());
            agent::run(
                &config,
                &store,
                &provider,
                cli.prompt.as_deref(),
                cli.model.as_deref(),
                cli.resume.as_deref(),
                cli.dangerously_skip_permissions,
                &cli.add_dir,
                cli.debug,
            )
            .await?;
        }
    }
    Ok(())
}
