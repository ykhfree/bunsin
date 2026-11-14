//! `bunsind`: the headless command line of the Bunsin engine.

use bunsin_api::{Edit, EngineApi, ProfileConfig};
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxBody;
use bunsin_daemon::RealBackends;
use bunsin_engine::Engine;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "bunsind", about = "Runs and controls Bunsin persona agents")]
struct Cli {
    /// The folder that holds all profiles.
    #[arg(long, env = "BUNSIN_DATA_DIR")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a profile that can do nothing until it gets settings.
    ProfileAdd {
        id: String,
        #[arg(long)]
        owner: String,
    },
    /// List all profiles.
    ProfileList,
    /// Print the settings of a profile as JSON.
    ConfigShow { profile: String },
    /// Replace the settings of a profile with the JSON in a file.
    ConfigSet { profile: String, file: PathBuf },
    /// Show the kill switch and the inbox of a profile.
    Status { profile: String },
    /// Turn the kill switch of a profile on or off.
    Stop {
        profile: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Hand one event to the agent.
    Ingest {
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

async fn run(cli: Cli) -> Result<(), String> {
    std::fs::create_dir_all(&cli.data_dir)
        .map_err(|e| format!("cannot create the data directory: {e}"))?;
    let engine = Engine::open(
        &cli.data_dir,
        Arc::new(RealBackends::from_env()),
        Arc::new(now),
    )
    .map_err(|e| e.to_string())?;
    let text = |e: bunsin_api::ApiError| e.to_string();

    match cli.command {
        Command::ProfileAdd { id, owner } => {
            let info = engine
                .create_profile(&id, ProfileConfig::new(&owner))
                .await
                .map_err(text)?;
            println!("created profile {} for {}", info.id, info.display_name);
        }
        Command::ProfileList => {
            for profile in engine.list_profiles().await.map_err(text)? {
                println!("{}\t{}", profile.id, profile.display_name);
            }
        }
        Command::ConfigShow { profile } => {
            println!(
                "{}",
                json(&engine.profile_config(&profile).await.map_err(text)?)
            );
        }
        Command::ConfigSet { profile, file } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            let config: ProfileConfig = serde_json::from_str(&content)
                .map_err(|e| format!("the settings file is invalid: {e}"))?;
            engine
                .set_profile_config(&profile, config)
                .await
                .map_err(text)?;
            println!("settings of {profile} replaced");
        }
        Command::Status { profile } => {
            let snapshot = engine.snapshot(&profile).await.map_err(text)?;
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
        Command::Stop { profile, state } => {
            let on = engine
                .set_kill_switch(&profile, state == "on")
                .await
                .map_err(text)?;
            println!("kill switch: {}", if on { "on" } else { "off" });
        }
        Command::Ingest {
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
                json(&engine.ingest(&profile, event).await.map_err(text)?)
            );
        }
        Command::Approve {
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
                    &engine
                        .approve(&profile, inbox_id, edit)
                        .await
                        .map_err(text)?
                )
            );
        }
        Command::Reject {
            profile,
            inbox_id,
            reason,
        } => {
            engine
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
