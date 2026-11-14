//! `bunsind`: the Bunsin server and its command line.
//!
//! Operator commands (`serve`, `profile-add`, `token-*`) work on the data directory
//! of this machine. All other commands talk to a running server over HTTPS.

use bunsin_api::{Edit, EngineApi, ProfileConfig};
use bunsin_client::{RemoteConfig, RemoteEngine};
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxBody;
use bunsin_daemon::RealBackends;
use bunsin_daemon::server::{ServeConfig, serve};
use bunsin_engine::Engine;
use bunsin_store::AppStore;
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "bunsind", about = "Runs and controls Bunsin persona agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where the profiles are stored on this machine.
#[derive(Args)]
struct Local {
    #[arg(long, env = "BUNSIN_DATA_DIR")]
    data_dir: PathBuf,
}

/// Which server to talk to. The access token is read from `BUNSIN_TOKEN`.
#[derive(Args)]
struct Remote {
    /// For example https://bunsin.example.com
    #[arg(long, env = "BUNSIN_SERVER")]
    server: String,
    /// A PEM certificate to trust instead of the system roots.
    #[arg(long, env = "BUNSIN_CA_CERT")]
    ca_cert: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Operator: run the server until it gets SIGTERM or Ctrl-C.
    Serve {
        #[command(flatten)]
        local: Local,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        /// PEM certificate chain. Required with --tls-key unless --listen is a loopback address.
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Let profile settings name commands that run on this machine as MCP servers.
        #[arg(long)]
        allow_stdio_connectors: bool,
    },
    /// Operator: create a profile that can do nothing until it gets settings.
    ProfileAdd {
        #[command(flatten)]
        local: Local,
        id: String,
        #[arg(long)]
        owner: String,
    },
    /// Operator: create an access token for a profile and print it once.
    TokenAdd {
        #[command(flatten)]
        local: Local,
        profile: String,
        /// What the token is for, for example "Kim's laptop".
        #[arg(long)]
        label: String,
    },
    /// Operator: list access tokens. Token values are not stored and cannot be shown.
    TokenList {
        #[command(flatten)]
        local: Local,
    },
    /// Operator: remove an access token by the ID that `token-list` shows.
    TokenRevoke {
        #[command(flatten)]
        local: Local,
        id: String,
    },
    /// List the profiles the token gives access to.
    ProfileList {
        #[command(flatten)]
        remote: Remote,
    },
    /// Print the settings of a profile as JSON.
    ConfigShow {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Replace the settings of a profile with the JSON in a file.
    ConfigSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        file: PathBuf,
    },
    /// Show the kill switch and the inbox of a profile.
    Status {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Turn the kill switch of a profile on or off.
    Stop {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Hand one event to the agent.
    Ingest {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        event_id: String,
        #[arg(long)]
        author: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        thread: Option<String>,
    },
    /// Approve a pending tool call and run it. `--args` with `--reason` edits it first.
    Approve {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        inbox_id: i64,
        /// The full edited arguments as a JSON object.
        #[arg(long, requires = "reason")]
        args: Option<String>,
        /// Why the draft was changed.
        #[arg(long, requires = "args")]
        reason: Option<String>,
    },
    /// Dismiss a pending item or a draft.
    Reject {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        inbox_id: i64,
        #[arg(long)]
        reason: String,
    },
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("API values serialize")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_store(local: &Local) -> Result<AppStore, String> {
    std::fs::create_dir_all(&local.data_dir)
        .map_err(|e| format!("cannot create the data directory: {e}"))?;
    AppStore::open(&local.data_dir).map_err(|e| e.to_string())
}

fn connect(remote: &Remote) -> Result<RemoteEngine, String> {
    let ca_cert_pem = match &remote.ca_cert {
        Some(path) => {
            Some(std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?)
        }
        None => None,
    };
    // The token comes from the environment only. A command-line flag would show in the process list.
    let token = std::env::var("BUNSIN_TOKEN")
        .map_err(|_| "set BUNSIN_TOKEN to your access token".to_owned())?;
    RemoteEngine::new(
        &RemoteConfig {
            url: remote.server.clone(),
            ca_cert_pem,
        },
        &token,
    )
    .map_err(|e| e.to_string())
}

async fn run_server(
    local: Local,
    listen: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
    allow_stdio: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(&local.data_dir)
        .map_err(|e| format!("cannot create the data directory: {e}"))?;
    let backends = Arc::new(RealBackends::from_env(allow_stdio));
    let engine =
        Engine::open(&local.data_dir, backends, Arc::new(now)).map_err(|e| e.to_string())?;
    let running = serve(Arc::new(engine), ServeConfig { listen, tls })
        .await
        .map_err(|e| e.to_string())?;
    // The handler is installed before the address is announced, so a stop request
    // that follows the announcement is never lost.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("cannot install the SIGTERM handler: {e}"))?;
    // Whoever started the server may have closed its output. That must not stop the server.
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "listening on {}", running.url());
    let _ = stdout.flush();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    let _ = writeln!(stdout, "stopping: waiting for running requests");
    running.shutdown().await.map_err(|e| e.to_string())
}

async fn run(cli: Cli) -> Result<(), String> {
    let text = |e: bunsin_api::ApiError| e.to_string();
    match cli.command {
        Command::Serve {
            local,
            listen,
            tls_cert,
            tls_key,
            allow_stdio_connectors,
        } => {
            run_server(local, listen, tls_cert.zip(tls_key), allow_stdio_connectors).await?;
        }
        Command::ProfileAdd { local, id, owner } => {
            let store = open_store(&local)?;
            let info =
                bunsin_engine::create_profile(&store, &id, &ProfileConfig::new(&owner), now())
                    .map_err(text)?;
            println!("created profile {} for {}", info.id, info.display_name);
        }
        Command::TokenAdd {
            local,
            profile,
            label,
        } => {
            let token = open_store(&local)?
                .add_token(&profile, &label, now())
                .map_err(|e| e.to_string())?;
            println!("{token}");
            eprintln!(
                "This token gives access to profile {profile}. It is shown once and is not stored."
            );
        }
        Command::TokenList { local } => {
            for token in open_store(&local)?
                .list_tokens()
                .map_err(|e| e.to_string())?
            {
                println!("{}\t{}\t{}", token.id, token.profile, token.label);
            }
        }
        Command::TokenRevoke { local, id } => {
            match open_store(&local)?
                .revoke_token(&id)
                .map_err(|e| e.to_string())?
            {
                0 => return Err(format!("no token has the ID `{id}`")),
                _ => println!("token {id} removed"),
            }
        }
        Command::ProfileList { remote } => {
            for profile in connect(&remote)?.list_profiles().await.map_err(text)? {
                println!("{}\t{}", profile.id, profile.display_name);
            }
        }
        Command::ConfigShow { remote, profile } => {
            println!(
                "{}",
                json(
                    &connect(&remote)?
                        .profile_config(&profile)
                        .await
                        .map_err(text)?
                )
            );
        }
        Command::ConfigSet {
            remote,
            profile,
            file,
        } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            let config: ProfileConfig = serde_json::from_str(&content)
                .map_err(|e| format!("the settings file is invalid: {e}"))?;
            connect(&remote)?
                .set_profile_config(&profile, config)
                .await
                .map_err(text)?;
            println!("settings of {profile} replaced");
        }
        Command::Status { remote, profile } => {
            let snapshot = connect(&remote)?.snapshot(&profile).await.map_err(text)?;
            println!(
                "kill switch: {}",
                if snapshot.kill_switch { "on" } else { "off" }
            );
            println!("inbox: {} item(s)", snapshot.inbox.len());
            for item in &snapshot.inbox {
                let what = match &item.body {
                    InboxBody::ToolCall { tool, args } => {
                        format!("{tool} {}", serde_json::Value::Object(args.clone()))
                    }
                    InboxBody::Escalation { summary } => format!("escalation: {summary}"),
                };
                println!("  #{} [{}] {what}", item.id, item.state.as_str());
            }
        }
        Command::Stop {
            remote,
            profile,
            state,
        } => {
            let on = connect(&remote)?
                .set_kill_switch(&profile, state == "on")
                .await
                .map_err(text)?;
            println!("kill switch: {}", if on { "on" } else { "off" });
        }
        Command::Ingest {
            remote,
            profile,
            source,
            event_id,
            author,
            text: body,
            thread,
        } => {
            let event = InboundEvent {
                source,
                external_id: event_id,
                thread,
                author,
                text: body,
            };
            println!(
                "{}",
                json(
                    &connect(&remote)?
                        .ingest(&profile, event)
                        .await
                        .map_err(text)?
                )
            );
        }
        Command::Approve {
            remote,
            profile,
            inbox_id,
            args,
            reason,
        } => {
            let edit = match (args, reason) {
                (Some(args), Some(reason)) => {
                    let args = serde_json::from_str(&args)
                        .map_err(|e| format!("--args is not a JSON object: {e}"))?;
                    Some(Edit { args, reason })
                }
                _ => None,
            };
            println!(
                "{}",
                json(
                    &connect(&remote)?
                        .approve(&profile, inbox_id, edit)
                        .await
                        .map_err(text)?
                )
            );
        }
        Command::Reject {
            remote,
            profile,
            inbox_id,
            reason,
        } => {
            connect(&remote)?
                .reject(&profile, inbox_id, &reason)
                .await
                .map_err(text)?;
            println!("item {inbox_id} dismissed");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
