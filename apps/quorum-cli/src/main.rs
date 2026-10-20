//! `quorum-cli`: headless access to the same commands the app runs.
//!
//! ```text
//! quorum-cli login [--device] [--tenant ID --client-id ID]   sign in (browser, or device code)
//! quorum-cli whoami                                          the signed-in account
//! quorum-cli permissions                                     granted and missing Graph scopes
//! quorum-cli sync [--seconds N]                              sync for N seconds, print status
//! quorum-cli run <command> ['{json}']                        run a command
//! quorum-cli query <query> ['{json}']                        run a query
//! quorum-cli commands | queries                              list commands / queries
//! quorum-cli mcp [--attach PORT] [--allow-destructive]       serve MCP on stdio
//! quorum-cli setup-tenant [--shell bash|powershell]          print the app registration script
//! quorum-cli consent-link                                    the admin-consent and setup links
//! ```
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]

use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use quorum_engine::{Session, SessionOptions};
use quorum_mcp::{Attached, Backend, DestructivePolicy, Server};
use serde_json::{Value, json};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Longest `sync --seconds`.
const MAX_SYNC_SECONDS: u64 = 3600;

#[derive(Parser, Debug)]
#[command(name = "quorum-cli", version, about = "Headless Quorum")]
struct Cli {
    /// Use recorded Graph fixtures instead of the network.
    #[arg(long, global = true)]
    mock_graph: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Sign in: opens the browser, or prints a device code with `--device`.
    Login {
        /// Sign in with a device code (for terminals without a browser).
        #[arg(long)]
        device: bool,
        /// Directory (tenant) id to save first.
        #[arg(long, requires = "client_id")]
        tenant: Option<String>,
        /// Application (client) id to save first.
        #[arg(long, requires = "tenant")]
        client_id: Option<String>,
    },
    /// Show the signed-in account.
    Whoami,
    /// List granted and missing Microsoft Graph permissions.
    Permissions,
    /// Start sync, wait, then print sync status.
    Sync {
        /// How long to let sync run (seconds).
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// List every command.
    Commands,
    /// List every query.
    Queries,
    /// Run a command: `quorum-cli run app.about '{}'`.
    Run {
        /// Command id.
        id: String,
        /// JSON params.
        #[arg(default_value = "null")]
        params: String,
    },
    /// Run a query: `quorum-cli query app.commands`.
    Query {
        /// Query id.
        id: String,
        /// JSON params.
        #[arg(default_value = "null")]
        params: String,
    },
    /// Print the script that registers Quorum in your Entra tenant (for the IT admin).
    SetupTenant {
        /// `bash` (macOS, Linux, Azure Cloud Shell) or `powershell` (Windows).
        #[arg(long, default_value = "bash")]
        shell: String,
    },
    /// Print the admin-consent link and the one-click setup link for the saved tenant.
    ConsentLink,
    /// Serve MCP on stdio, headless or attached to a running app.
    Mcp {
        /// Drive the running app on this control port instead of a headless session.
        #[arg(long)]
        attach: Option<u16>,
        /// Let agents run send/post/delete/share commands without `"confirm": true`.
        #[arg(long)]
        allow_destructive: bool,
    },
}

fn engine(e: quorum_engine::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// The destructive-command policy: the flag, else Settings ▸ Agents.
async fn policy(session: &Session, allow: bool) -> DestructivePolicy {
    if allow {
        return DestructivePolicy::Allow;
    }
    match quorum_engine::settings::get(session, "agents.destructive_policy").await.ok().as_ref().and_then(Value::as_str)
    {
        Some("allow") => DestructivePolicy::Allow,
        Some("deny") => DestructivePolicy::Deny,
        _ => DestructivePolicy::RequireConfirm,
    }
}

/// Device-code sign-in: prints the code to stderr and waits; tokens go to the keychain.
async fn device_login() -> anyhow::Result<()> {
    let cfg = quorum_engine::account::AccountConfig::load()
        .context("set the tenant and client id first: quorum-cli login --tenant ID --client-id ID")?;
    let cfg = quorum_auth::AuthConfig::builder(cfg.client_id).tenant(cfg.tenant).build()?;
    let auth = quorum_auth::Authenticator::new(cfg)?;
    let code = auth.start_device_code().await?;
    eprintln!("{}", code.message());
    let provider = auth.finish_device_code(&code).await?;
    eprintln!("Signed in as {}.", provider.account().username());
    Ok(())
}

async fn login(
    session: &Session,
    device: bool,
    tenant: Option<String>,
    client_id: Option<String>,
) -> anyhow::Result<Value> {
    if let (Some(tenant), Some(client_id)) = (tenant, client_id) {
        session
            .execute("account.configure", json!({ "tenant": tenant, "client_id": client_id }))
            .await
            .map_err(engine)?;
    }
    if device && !session.options().mock_graph {
        device_login().await?;
        // A fresh session restores the new sign-in from the keychain.
        let fresh = Session::new(SessionOptions::default()).map_err(engine)?;
        fresh.start().await;
        return fresh.query("account.current", Value::Null).await.map_err(engine);
    }
    session.execute("account.sign_in", Value::Null).await.map_err(engine)?;
    session.query("account.current", Value::Null).await.map_err(engine)
}

async fn sync(session: &Session, seconds: u64) -> anyhow::Result<Value> {
    let account = session.query("account.current", Value::Null).await.map_err(engine)?;
    let handle =
        session.sync().map_err(|_| anyhow::anyhow!("not signed in: run `quorum-cli login` (or use --mock-graph)"))?;
    // Ask for an immediate pass where the command exists; sync also runs on its own cadence.
    let _ = session.execute("sync.now", Value::Null).await;
    tokio::time::sleep(Duration::from_secs(seconds.min(MAX_SYNC_SECONDS))).await;
    let status = match session.query("sync.status", Value::Null).await {
        Ok(v) => v,
        Err(_) => serde_json::to_value(handle.status())?,
    };
    Ok(json!({ "account": account, "waited_seconds": seconds.min(MAX_SYNC_SECONDS), "status": status }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    // Setup output needs no session (and must stay byte-identical to the committed scripts).
    if let Cmd::SetupTenant { shell } = &cli.command {
        let script = match shell.as_str() {
            "bash" | "sh" | "zsh" => quorum_engine::registration::bash_script(),
            "powershell" | "pwsh" | "ps1" => quorum_engine::registration::powershell_script(),
            other => anyhow::bail!("unknown shell `{other}`: use bash or powershell"),
        };
        std::io::stdout().write_all(script.as_bytes())?;
        return Ok(());
    }
    if let Cmd::ConsentLink = cli.command {
        let cfg = quorum_engine::account::AccountConfig::load()
            .context("no tenant saved yet: run `quorum-cli login --tenant ID --client-id ID` first")?;
        let consent = quorum_engine::registration::admin_consent_url(&cfg.tenant, &cfg.client_id)
            .context("the saved tenant or client id is not valid")?;
        let setup = quorum_engine::registration::setup_link(&cfg.tenant, &cfg.client_id).unwrap_or_default();
        let out = json!({ "admin_consent": consent, "setup_link": setup });
        writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&out)?)?;
        return Ok(());
    }
    let session = Session::new(SessionOptions { mock_graph: cli.mock_graph }).map_err(engine)?;
    session.start().await;
    if let Cmd::Mcp { attach, allow_destructive } = cli.command {
        let policy = policy(&session, allow_destructive).await;
        let backend = match attach {
            Some(port) => {
                let token =
                    std::fs::read_to_string(quorum_control::token_path()).context("reading the control token")?;
                Backend::Attached(Attached::connect(port, token.trim()).await?)
            }
            None => Backend::Local(session),
        };
        Server::new(backend, policy).serve_stdio().await?;
        return Ok(());
    }
    let out = match cli.command {
        Cmd::Login { device, tenant, client_id } => login(&session, device, tenant, client_id).await?,
        Cmd::Whoami => session.query("account.current", Value::Null).await.map_err(engine)?,
        Cmd::Permissions => session.query("account.permissions", Value::Null).await.map_err(engine)?,
        Cmd::Sync { seconds } => sync(&session, seconds).await?,
        Cmd::Commands => serde_json::to_value(session.commands())?,
        Cmd::Queries => session.query("app.queries", Value::Null).await.map_err(engine)?,
        Cmd::Run { id, params } => {
            let params = serde_json::from_str(&params).context("params must be JSON")?;
            session.execute(&id, params).await.map_err(engine)?
        }
        Cmd::Query { id, params } => {
            let params = serde_json::from_str(&params).context("params must be JSON")?;
            session.query(&id, params).await.map_err(engine)?
        }
        Cmd::Mcp { .. } | Cmd::SetupTenant { .. } | Cmd::ConsentLink => return Ok(()),
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &out)?;
    writeln!(stdout)?;
    Ok(())
}
