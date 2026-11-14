//! `bunsind`: the Bunsin server and its command line.
//!
//! Operator commands (`serve`, `profile-add`, `token-*`) work on the data directory
//! of this machine. All other commands talk to a running server over HTTPS.

use bunsin_api::{ConnectorUpdate, Edit, EngineApi, LlmUpdate, ProfileConfig, TokenChange, TokenSource};
use bunsin_client::{RemoteConfig, RemoteEngine};
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxBody;
use bunsin_daemon::server::{Poller, ServeConfig, serve};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
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

/// What to do with the token of a connection. Without an option the token is kept.
#[derive(Args)]
struct TokenArgs {
    /// Read the token from standard input. A token on the command line would show in the process list.
    #[arg(long, conflicts_with = "clear_token")]
    token_stdin: bool,
    /// Remove the stored token.
    #[arg(long)]
    clear_token: bool,
}

impl TokenArgs {
    fn change(&self) -> Result<TokenChange, String> {
        if self.clear_token {
            return Ok(TokenChange::Clear);
        }
        if !self.token_stdin {
            return Ok(TokenChange::Keep);
        }
        let mut token = String::new();
        std::io::stdin()
            .read_line(&mut token)
            .map_err(|e| format!("cannot read the token: {e}"))?;
        Ok(TokenChange::Set(token.trim().to_owned()))
    }
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
        /// An LLM or MCP host that profile settings may name. Repeat it for more hosts.
        /// With this option, only the listed hosts are allowed. Without it, any https://
        /// address is allowed except localhost and internal IP addresses; a host name that
        /// resolves to an internal address still passes. Give the list on a shared server.
        #[arg(long = "allow-upstream-host")]
        allow_upstream_hosts: Vec<String>,
        /// The file with the key that encrypts tokens which owners store on the server.
        /// It is created if it does not exist. The default is `master.key` in the data
        /// directory. Keep this file out of backups of the data directory.
        #[arg(long, env = "BUNSIN_MASTER_KEY_FILE")]
        master_key_file: Option<PathBuf>,
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
    /// List the tools of a connector as its MCP server describes them now.
    /// With --approve, approve exactly that list.
    Tools {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        connector: String,
        #[arg(long)]
        approve: bool,
        /// Print the full tool descriptions and input schemas as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run one poll of the settings once and print the events it would hand to the agent.
    /// Nothing is stored and no event is handled.
    PollTest {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        /// The position of the poll in the `polls` list of the settings, from 0.
        index: usize,
    },
    /// Show the LLM endpoint and the connectors, and where each token comes from.
    Connections {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Save the LLM endpoint. With --token-stdin, read its token from standard input.
    LlmSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        #[arg(long, value_parser = ["anthropic-messages", "openai-chat"])]
        wire: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        model: String,
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        #[command(flatten)]
        token: TokenArgs,
    },
    /// Save a connector that is reached over HTTP. With --token-stdin, read its token
    /// from standard input. Then check it with `tools`.
    ConnectorSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        name: String,
        #[arg(long)]
        url: String,
        #[command(flatten)]
        token: TokenArgs,
    },
    /// Remove a connector and its stored token.
    ConnectorRemove {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        name: String,
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
    std::fs::create_dir_all(&local.data_dir).map_err(|e| format!("cannot create the data directory: {e}"))?;
    AppStore::open(&local.data_dir).map_err(|e| e.to_string())
}

fn connect(remote: &Remote) -> Result<RemoteEngine, String> {
    let ca_cert_pem = match &remote.ca_cert {
        Some(path) => Some(std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?),
        None => None,
    };
    // The token comes from the environment only. A command-line flag would show in the process list.
    let token = std::env::var("BUNSIN_TOKEN").map_err(|_| "set BUNSIN_TOKEN to your access token".to_owned())?;
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
    backends: RealBackends,
    master_key_file: Option<PathBuf>,
) -> Result<(), String> {
    std::fs::create_dir_all(&local.data_dir).map_err(|e| format!("cannot create the data directory: {e}"))?;
    let backends = Arc::new(backends);
    let key_file = master_key_file.unwrap_or_else(|| local.data_dir.join("master.key"));
    let engine =
        Engine::open_with_key_file(&local.data_dir, &key_file, backends, Arc::new(now)).map_err(|e| e.to_string())?;
    let engine = Arc::new(engine);
    let running = serve(engine.clone(), ServeConfig { listen, tls })
        .await
        .map_err(|e| e.to_string())?;
    let poller = Poller::start(engine);
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
    poller.stop().await;
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
            allow_upstream_hosts,
            master_key_file,
        } => {
            let allowed_hosts = allow_upstream_hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect();
            let backends = RealBackends::from_env(allow_stdio_connectors, UpstreamPolicy { allowed_hosts });
            run_server(local, listen, tls_cert.zip(tls_key), backends, master_key_file).await?;
        }
        Command::ProfileAdd { local, id, owner } => {
            let store = open_store(&local)?;
            let info = bunsin_engine::create_profile(&store, &id, &ProfileConfig::new(&owner), now()).map_err(text)?;
            println!("created profile {} for {}", info.id, info.display_name);
        }
        Command::TokenAdd { local, profile, label } => {
            let token = open_store(&local)?
                .add_token(&profile, &label, now())
                .map_err(|e| e.to_string())?;
            println!("{token}");
            eprintln!("This token gives access to profile {profile}. It is shown once and is not stored.");
        }
        Command::TokenList { local } => {
            for token in open_store(&local)?.list_tokens().map_err(|e| e.to_string())? {
                println!("{}\t{}\t{}", token.id, token.profile, token.label);
            }
        }
        Command::TokenRevoke { local, id } => {
            match open_store(&local)?.revoke_token(&id).map_err(|e| e.to_string())? {
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
                json(&connect(&remote)?.profile_config(&profile).await.map_err(text)?)
            );
        }
        Command::ConfigSet { remote, profile, file } => {
            let content = std::fs::read_to_string(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            let config: ProfileConfig =
                serde_json::from_str(&content).map_err(|e| format!("the settings file is invalid: {e}"))?;
            connect(&remote)?
                .set_profile_config(&profile, config)
                .await
                .map_err(text)?;
            println!("settings of {profile} replaced");
        }
        Command::Status { remote, profile } => {
            let snapshot = connect(&remote)?.snapshot(&profile).await.map_err(text)?;
            println!("kill switch: {}", if snapshot.kill_switch { "on" } else { "off" });
            for poll in &snapshot.polls {
                let state = match (&poll.error, poll.last_run_at) {
                    (Some(error), _) => format!("error: {error}"),
                    (None, None) => "has not run yet".to_owned(),
                    // Items came back, but none had an ID, text and an author.
                    (None, Some(_)) if poll.items > 0 && poll.usable == 0 => {
                        format!("{} item(s), none usable: check the mapping in the settings", poll.items)
                    }
                    (None, Some(_)) => format!("{} item(s), {} new event(s)", poll.items, poll.new_events),
                };
                println!("poll {}: {state}", poll.tool);
            }
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
        Command::Stop { remote, profile, state } => {
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
            let outcome = connect(&remote)?.ingest(&profile, event).await.map_err(text)?;
            // A handling that failed is an error for the caller, who may hand the event in again.
            if let bunsin_api::Outcome::Failed { error } = outcome {
                return Err(format!("the event was not handled: {error}"));
            }
            println!("{}", json(&outcome));
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
                    let args = serde_json::from_str(&args).map_err(|e| format!("--args is not a JSON object: {e}"))?;
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
        Command::Tools {
            remote,
            profile,
            connector,
            approve,
            json: as_json,
        } => {
            let engine = connect(&remote)?;
            let report = engine.connector_tools(&profile, &connector).await.map_err(text)?;
            if as_json {
                println!("{}", json(&report.tools));
            } else {
                for tool in &report.tools {
                    let state = if report.diff.added.contains(&tool.name) {
                        "new"
                    } else if report.diff.changed.contains(&tool.name) {
                        "changed"
                    } else {
                        "approved"
                    };
                    let summary = tool.description.lines().next().unwrap_or_default();
                    println!("{state}\t{connector}/{}\t{summary}", tool.name);
                }
                for name in &report.diff.removed {
                    println!("gone\t{connector}/{name}");
                }
            }
            if approve {
                let shown = bunsin_core::tool::pin(&report.tools);
                engine.approve_tools(&profile, &connector, shown).await.map_err(text)?;
                eprintln!(
                    "approved {} tool(s) of {connector}. A tool also needs a rule in `policy.rules` \
                     before the agent can use it.",
                    report.tools.len()
                );
            }
        }
        Command::Connections { remote, profile } => {
            let connections = connect(&remote)?.connections(&profile).await.map_err(text)?;
            let source = |token: TokenSource| match token {
                TokenSource::Stored => "token stored on the server",
                TokenSource::ServerEnvironment => "token from the server's environment",
                TokenSource::None => "no token",
            };
            match &connections.llm {
                Some(llm) => println!(
                    "llm\t{}\t{}\t{}\t{}",
                    json(&llm.wire),
                    llm.base_url,
                    llm.model,
                    source(llm.token)
                ),
                None => println!("llm\tnot set"),
            }
            for (name, connector) in &connections.connectors {
                let address = match &connector.mcp {
                    bunsin_core::settings::McpConfig::Http { url } => url.clone(),
                    bunsin_core::settings::McpConfig::Stdio { command, .. } => format!("command {}", command.display()),
                };
                println!("{name}\t{address}\t{}", source(connector.token));
            }
        }
        Command::LlmSet {
            remote,
            profile,
            wire,
            base_url,
            model,
            timeout_secs,
            token,
        } => {
            let wire = serde_json::from_value(serde_json::Value::String(wire)).map_err(|e| e.to_string())?;
            let update = LlmUpdate {
                wire,
                base_url,
                model,
                timeout_secs,
                token: token.change()?,
            };
            connect(&remote)?.set_llm(&profile, update).await.map_err(text)?;
            println!("LLM endpoint of {profile} saved");
        }
        Command::ConnectorSet {
            remote,
            profile,
            name,
            url,
            token,
        } => {
            let update = ConnectorUpdate {
                url,
                token: token.change()?,
            };
            connect(&remote)?
                .set_connector(&profile, &name, update)
                .await
                .map_err(text)?;
            println!("connector {name} of {profile} saved. Check it with: bunsind tools {profile} {name}");
        }
        Command::ConnectorRemove { remote, profile, name } => {
            connect(&remote)?
                .remove_connector(&profile, &name)
                .await
                .map_err(text)?;
            println!("connector {name} of {profile} removed");
        }
        Command::PollTest { remote, profile, index } => {
            let preview = connect(&remote)?.test_poll(&profile, index).await.map_err(text)?;
            println!(
                "{} item(s) in the result, {} usable event(s)",
                preview.items,
                preview.events.len()
            );
            println!("{}", json(&preview.events));
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
