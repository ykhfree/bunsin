//! `bunsind`: the Bunsin server and its command line.
//!
//! Operator commands (`serve`, `profile-add`, `token-*`) work on the data directory
//! of this machine. All other commands talk to a running server over HTTPS.

use bunsin_api::{
    ConnectorUpdate, Edit, EngineApi, LlmUpdate, ProfileConfig, TokenChange, TokenSource, WikiPageUpdate,
};
use bunsin_client::{RemoteConfig, RemoteEngine};
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxBody;
use bunsin_core::settings::Wire;
use bunsin_daemon::keystore::KeyStore;
use bunsin_daemon::server::{ServeConfig, start};
use bunsin_daemon::{RealBackends, UpstreamPolicy, rules};
use bunsin_engine::Engine;
use bunsin_engine::{KeyFile, KeySource};
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
        /// Every profile owner can then start any program as the user of this server.
        /// Use it only if you trust every profile owner.
        #[arg(long)]
        allow_stdio_connectors: bool,
        /// An LLM or MCP host that profile settings may name. Repeat it for more hosts.
        /// With this option, only the listed hosts are allowed. Without it, any https://
        /// address is allowed except localhost and internal IP addresses; a host name that
        /// resolves to an internal address still passes. Give the list on a shared server.
        #[arg(long = "allow-upstream-host")]
        allow_upstream_hosts: Vec<String>,
        /// Where the key is kept that encrypts tokens which owners store on the server.
        /// `keychain`: the login Keychain of this Mac. `file`: a key file.
        /// The default is `keychain` on macOS and `file` on other systems. A server on a Mac
        /// that starts without a login session needs `file`.
        #[arg(long, env = "BUNSIN_KEY_STORE")]
        key_store: Option<String>,
        /// Start with a new key although the data directory holds encrypted tokens and their
        /// key is lost. The notes and the settings stay. Every stored token becomes unreadable,
        /// and its owner enters it again. A key that exists is used and is not replaced.
        #[arg(long)]
        accept_new_key: bool,
        /// The key file of the file store. It is created if it does not exist. The default is
        /// `master.key` in the data directory. Keep this file out of backups of the data directory.
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
    /// Send one test request to the saved LLM endpoint. It costs tokens at the endpoint.
    LlmTest {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Show the LLM calls of the last 90 days, added up by UTC day and purpose.
    LlmUsage {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Save the LLM endpoint. With --token-stdin, read its token from standard input.
    LlmSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        #[arg(long, value_parser = ["anthropic-messages", "openai-chat", "openai-responses"], default_value = "anthropic-messages")]
        wire: String,
        /// With the format anthropic-messages, the default is the Claude API.
        /// The other formats have no default.
        #[arg(long)]
        base_url: Option<String>,
        /// With the format anthropic-messages, the default is a Claude model.
        /// The other formats have no default.
        #[arg(long)]
        model: Option<String>,
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
        /// The header that carries the token as it is, for example X-API-Key.
        /// Without it, the token goes as `Authorization: Bearer <token>`.
        /// Give it with every `connector-set` of this connector: a call without it sets Bearer again.
        #[arg(long)]
        token_header: Option<String>,
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
    /// List the pages of the agent's notes.
    WikiList {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Print one page of the notes with its sources.
    WikiShow {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        slug: String,
    },
    /// Write a page of the notes yourself. The body is read from a file.
    WikiSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        slug: String,
        #[arg(long)]
        title: String,
        /// One line for the index.
        #[arg(long)]
        summary: String,
        /// A markdown file with the body of the page.
        #[arg(long)]
        body_file: PathBuf,
    },
    /// List every stored revision of a page. With --revision, print that revision's text,
    /// which you can save again with `wiki-set` to restore it.
    WikiHistory {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        slug: String,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Delete a page of the notes.
    WikiDelete {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        slug: String,
    },
    /// Write the notes as markdown files into a folder: `index.md` and one file per page.
    WikiExport {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        dir: PathBuf,
    },
    /// Show the permission table: one line per rule, and `off` for an approved tool without a rule.
    Rules {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Set the rule of one tool. The other rules stay. A write or delete rule needs
    /// --text or --no-text, so that a text without the AI marker is a choice, not a default.
    RuleSet {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        /// <connector>/<tool>
        tool: String,
        #[arg(long, value_parser = ["read", "write", "delete"])]
        class: String,
        /// How far a write goes. The default is approve. Not for a read rule.
        #[arg(long, value_parser = ["observe", "draft", "approve", "autonomous"])]
        tier: Option<String>,
        /// A place of text for other people: an argument name, or a path such as
        /// blocks.*.text, where `*` means every item of a list. Repeat it for more places.
        #[arg(long = "text", conflicts_with = "no_text")]
        texts: Vec<String>,
        /// The tool sends no text to other people.
        #[arg(long)]
        no_text: bool,
    },
    /// Remove the rule of one tool. The agent cannot use the tool then.
    RuleRemove {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        /// <connector>/<tool>
        tool: String,
    },
    /// Resolve an item whose outcome is unknown, because the server stopped while its call ran.
    /// Check the target system first. `done`: the call reached its tool. `drop`: give it up.
    /// `retry`: run a copy now as a new item. If the first call reached its tool, it is sent twice.
    ResolveUnknown {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        inbox_id: i64,
        #[arg(value_parser = ["done", "retry", "drop"])]
        resolution: String,
    },
    /// Show the stored corrections: the edits you made to the agent's drafts. Newest first.
    Corrections {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Retire a correction. It stays stored, but the agent no longer gets it.
    RetireCorrection {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        id: i64,
    },
    /// Show, for each write or delete tool, how often you approved, edited or dismissed its calls,
    /// how often it ran without asking, whether it is held, and whether it is suggested for autonomous.
    Autonomy {
        #[command(flatten)]
        remote: Remote,
        profile: String,
    },
    /// Report a problem with a call that ran without asking. Its tool then asks you for every call
    /// until you clear the hold.
    ReportAutonomous {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        inbox_id: i64,
    },
    /// Clear the hold of a tool. Its calls then follow its step again.
    ClearAutonomyHold {
        #[command(flatten)]
        remote: Remote,
        profile: String,
        /// <connector>/<tool>
        tool: String,
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

/// A page as a markdown file.
fn page_markdown(page: &bunsin_core::wiki::WikiPage) -> String {
    let mut text = format!(
        "# {}\n\n{}\n\n{}\n\n## Sources\n\n",
        page.text.title, page.text.summary, page.text.body
    );
    if page.sources.is_empty() {
        text.push_str("- written by the owner\n");
    }
    for source in &page.sources {
        text.push_str(&format!("- {}: {}\n", source.origin, source.detail));
    }
    text
}

/// A correction as indented lines. A line break in a text starts an indented line too.
fn correction_text(record: &bunsin_core::records::CorrectionRecord) -> String {
    let c = &record.correction;
    let retired = if record.retired_at.is_some() { " (retired)" } else { "" };
    let mut text = format!("#{} {} from {}{retired}\n", record.id, c.tool, c.source);
    for (label, value) in [
        ("task", &c.task),
        ("draft", &c.original),
        ("edit", &c.edited),
        ("reason", &c.reason),
    ] {
        text.push_str(&format!("  {label}: {}\n", value.replace('\n', "\n    ")));
    }
    text
}

/// The autonomy counts as a header line and one tab-separated line per tool.
fn autonomy_lines(autonomy: &bunsin_api::Autonomy) -> Vec<String> {
    let thresholds = match &autonomy.thresholds {
        None => "thresholds: none, so no tool is suggested; the counts cover all days".to_owned(),
        Some(t) => format!(
            "thresholds: {} reviewed, {}% unchanged, last {} days",
            t.min_reviewed, t.min_unchanged_percent, t.window_days
        ),
    };
    let mut lines = vec![
        thresholds,
        "tool\treviewed\tunchanged\tedited\trejected\tautonomous\tfailed\tstate".to_owned(),
    ];
    for tool in &autonomy.tools {
        let c = &tool.counts;
        let percent = c
            .unchanged_percent()
            .map_or_else(|| "-".to_owned(), |percent| format!("{percent}%"));
        let state = match (tool.held, tool.suggested) {
            (true, _) => "held",
            (false, true) => "suggested",
            (false, false) => "-",
        };
        lines.push(format!(
            "{}\t{}\t{} ({percent})\t{}\t{}\t{}\t{}\t{state}",
            tool.tool,
            c.reviewed(),
            c.approved,
            c.edited,
            c.rejected,
            c.autonomous,
            c.autonomous_failed
        ));
    }
    lines
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

/// The source of the master key for the chosen store.
fn key_source(store: &KeyStore, data_dir: &std::path::Path) -> Result<Box<dyn KeySource>, String> {
    match store {
        KeyStore::File(file) => Ok(Box::new(KeyFile(file.clone()))),
        #[cfg(target_os = "macos")]
        KeyStore::Keychain => Ok(Box::new(bunsin_daemon::keystore::KeychainKey::new(
            data_dir,
            bunsin_daemon::keystore::MacKeychainItem::boxed,
        ))),
        // `key_store` gives this store only on macOS.
        #[cfg(not(target_os = "macos"))]
        KeyStore::Keychain => {
            let _ = data_dir;
            Err("--key-store keychain works on macOS only".to_owned())
        }
    }
}

async fn run_server(
    local: Local,
    listen: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
    backends: RealBackends,
    store: KeyStore,
    accept_new_key: bool,
) -> Result<(), String> {
    let config = ServeConfig { listen, tls };
    // A start that is refused for its address must not read or move the key.
    config.check().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&local.data_dir).map_err(|e| format!("cannot create the data directory: {e}"))?;
    let backends = Arc::new(backends);
    let source = key_source(&store, &local.data_dir)?;
    let source: Box<dyn KeySource> = if accept_new_key {
        Box::new(bunsin_daemon::keystore::AcceptNewKey(source))
    } else {
        source
    };
    let engine =
        Engine::open_with_key(&local.data_dir, source.as_ref(), backends, Arc::new(now)).map_err(|e| e.to_string())?;
    let engine = Arc::new(engine);
    let (running, poller) = start(engine, config).await.map_err(|e| e.to_string())?;
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
            key_store,
            accept_new_key,
            master_key_file,
        } => {
            let allowed_hosts = allow_upstream_hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect();
            let backends = RealBackends::from_env(allow_stdio_connectors, UpstreamPolicy { allowed_hosts });
            let store = bunsin_daemon::keystore::key_store(
                std::env::consts::OS,
                key_store.as_deref(),
                master_key_file,
                &local.data_dir,
            )?;
            run_server(local, listen, tls_cert.zip(tls_key), backends, store, accept_new_key).await?;
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
                    InboxBody::WikiChange { slug, text, .. } => format!("note `{slug}`: {}", text.summary),
                };
                let retry = match item.retry_of {
                    Some(of) => format!(" (repeats #{of})"),
                    None => String::new(),
                };
                println!("  #{} [{}] {what}{retry}", item.id, item.state.as_str());
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
                    json(&llm.wire).trim_matches('"'),
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
                let header = match &connector.token_header {
                    Some(header) => format!("\tin the header {header}"),
                    None => String::new(),
                };
                println!("{name}\t{address}\t{}{header}", source(connector.token));
            }
        }
        Command::LlmTest { remote, profile } => {
            let test = connect(&remote)?.test_llm(&profile).await.map_err(text)?;
            if !test.works {
                return Err(format!("the endpoint does not work: {}", test.detail));
            }
            println!("works: {}", test.detail);
        }
        Command::LlmUsage { remote, profile } => {
            println!("day\tpurpose\tcalls\tfailed\tinput tokens\toutput tokens\taverage ms");
            for usage in connect(&remote)?.llm_usage(&profile).await.map_err(text)? {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    usage.day,
                    usage.purpose,
                    usage.calls,
                    usage.failed,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.latency_ms / usage.calls.max(1)
                );
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
            let wire: Wire = serde_json::from_value(serde_json::Value::String(wire)).map_err(|e| e.to_string())?;
            // The Claude address and model fit only the Claude format.
            let default = |value: Option<String>, name: &str, claude: &str| match (value, wire) {
                (Some(value), _) => Ok(value),
                (None, Wire::AnthropicMessages) => Ok(claude.to_owned()),
                (None, _) => Err(format!("give {name}: this format has no default")),
            };
            let update = LlmUpdate {
                wire,
                base_url: default(base_url, "--base-url", bunsin_core::settings::ANTHROPIC_BASE_URL)?,
                model: default(model, "--model", bunsin_core::settings::ANTHROPIC_DEFAULT_MODEL)?,
                timeout_secs,
                token: token.change()?,
                expected: bunsin_api::Expected::Unchecked,
            };
            connect(&remote)?.set_llm(&profile, update).await.map_err(text)?;
            println!("LLM endpoint of {profile} saved");
        }
        Command::ConnectorSet {
            remote,
            profile,
            name,
            url,
            token_header,
            token,
        } => {
            let update = ConnectorUpdate {
                url,
                token: token.change()?,
                token_header,
                expected: bunsin_api::Expected::Unchecked,
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
        Command::WikiList { remote, profile } => {
            for page in connect(&remote)?.wiki_index(&profile).await.map_err(text)? {
                println!("{}\t{}\t{}", page.slug, page.title, page.summary);
            }
        }
        Command::WikiShow { remote, profile, slug } => {
            let page = connect(&remote)?.wiki_page(&profile, &slug).await.map_err(text)?;
            println!("{}", page_markdown(&page));
        }
        Command::WikiSet {
            remote,
            profile,
            slug,
            title,
            summary,
            body_file,
        } => {
            let body =
                std::fs::read_to_string(&body_file).map_err(|e| format!("cannot read {}: {e}", body_file.display()))?;
            let update = WikiPageUpdate {
                text: bunsin_core::wiki::WikiText { title, summary, body },
                expected_revision: None,
            };
            let page = connect(&remote)?
                .set_wiki_page(&profile, &slug, update)
                .await
                .map_err(text)?;
            println!("page {} saved at revision {}", page.slug, page.revision);
        }
        Command::WikiHistory {
            remote,
            profile,
            slug,
            revision,
        } => {
            let history = connect(&remote)?.wiki_history(&profile, &slug).await.map_err(text)?;
            match revision {
                Some(wanted) => {
                    let page = history.iter().flatten().find(|page| page.revision == wanted);
                    let page = page.ok_or_else(|| format!("page {slug} has no revision {wanted}"))?;
                    println!("{}", page_markdown(page));
                }
                None => {
                    for entry in &history {
                        match entry {
                            Some(page) => {
                                let author = json(&page.author);
                                println!("{}\t{}\t{}", page.revision, author.trim_matches('"'), page.text.summary);
                            }
                            None => println!("-\tdeleted"),
                        }
                    }
                }
            }
        }
        Command::WikiDelete { remote, profile, slug } => {
            connect(&remote)?
                .delete_wiki_page(&profile, &slug)
                .await
                .map_err(text)?;
            println!("page {slug} deleted");
        }
        Command::WikiExport { remote, profile, dir } => {
            let engine = connect(&remote)?;
            let index = engine.wiki_index(&profile).await.map_err(text)?;
            let pages_dir = dir.join("pages");
            std::fs::create_dir_all(&pages_dir).map_err(|e| format!("cannot create {}: {e}", pages_dir.display()))?;
            let mut lines = vec!["# Notes".to_owned(), String::new()];
            for entry in &index {
                let page = engine.wiki_page(&profile, &entry.slug).await.map_err(text)?;
                let path = pages_dir.join(format!("{}.md", page.slug));
                std::fs::write(&path, page_markdown(&page))
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                lines.push(format!(
                    "- [{}](pages/{}.md): {}",
                    entry.title, entry.slug, entry.summary
                ));
            }
            let path = dir.join("index.md");
            std::fs::write(&path, lines.join("\n") + "\n")
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("{} page(s) written to {}", index.len(), dir.display());
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
        Command::Rules { remote, profile } => {
            let permissions = connect(&remote)?.permissions(&profile).await.map_err(text)?;
            for line in rules::table_lines(&permissions) {
                println!("{line}");
            }
        }
        Command::RuleSet {
            remote,
            profile,
            tool,
            class,
            tier,
            texts,
            no_text,
        } => {
            // The options are checked before the server is asked.
            let rule = rules::parse_rule(&class, tier.as_deref(), &texts, no_text)?;
            rules::check_tool_id(&tool)?;
            rules::save_rule(&connect(&remote)?, &profile, &tool, Some(rule)).await?;
            println!("rule of {tool} in {profile} saved");
        }
        Command::RuleRemove { remote, profile, tool } => {
            rules::save_rule(&connect(&remote)?, &profile, &tool, None).await?;
            println!("rule of {tool} in {profile} removed");
        }
        Command::ResolveUnknown {
            remote,
            profile,
            inbox_id,
            resolution,
        } => {
            let resolution: bunsin_api::Resolution =
                serde_json::from_value(serde_json::Value::String(resolution)).map_err(|e| e.to_string())?;
            let engine = connect(&remote)?;
            match engine
                .resolve_unknown(&profile, inbox_id, resolution)
                .await
                .map_err(text)?
            {
                Some(outcome) => println!("{}", json(&outcome)),
                None => println!("item {inbox_id} resolved"),
            }
        }
        Command::Corrections { remote, profile } => {
            for record in connect(&remote)?.corrections(&profile).await.map_err(text)? {
                print!("{}", correction_text(&record));
            }
        }
        Command::RetireCorrection { remote, profile, id } => {
            connect(&remote)?.retire_correction(&profile, id).await.map_err(text)?;
            println!("correction {id} retired");
        }
        Command::Autonomy { remote, profile } => {
            for line in autonomy_lines(&connect(&remote)?.autonomy(&profile).await.map_err(text)?) {
                println!("{line}");
            }
        }
        Command::ReportAutonomous {
            remote,
            profile,
            inbox_id,
        } => {
            connect(&remote)?
                .report_autonomous(&profile, inbox_id)
                .await
                .map_err(text)?;
            println!("item {inbox_id} reported; its tool asks you until you clear the hold");
        }
        Command::ClearAutonomyHold { remote, profile, tool } => {
            connect(&remote)?
                .clear_autonomy_hold(&profile, &tool)
                .await
                .map_err(text)?;
            println!("hold of {tool} cleared");
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
