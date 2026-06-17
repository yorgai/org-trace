use anyhow::Result;
use clap::{Parser, Subcommand};
use org_trace_core::{discover_repo_root, LocalStore};
use org_trace_protocol::{ActorRef, ActorType, EventType, TraceEvent};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "orgii-trace")]
#[command(about = "Record and sync ORGII mission provenance events")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Mission {
        #[command(subcommand)]
        command: MissionCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
    Push {
        #[arg(long)]
        dry_run: bool,
    },
    Pull {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MissionCommand {
    Create { title: String },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Start {
        #[arg(long)]
        mission: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    Decision {
        #[arg(long)]
        mission: Option<String>,
        title: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo_root = discover_repo_root(std::env::current_dir()?)?;
    let store = LocalStore::new(repo_root);

    match cli.command {
        Command::Init => {
            store.init()?;
            println!("Initialized ORGII Trace at {}", store.provenance_dir().display());
        }
        Command::Mission { command } => handle_mission(command, &store)?,
        Command::Session { command } => handle_session(command, &store)?,
        Command::Artifact { command } => handle_artifact(command, &store)?,
        Command::Sync { dry_run } => println!("sync is not implemented yet; dry_run={dry_run}"),
        Command::Push { dry_run } => println!("push is not implemented yet; dry_run={dry_run}"),
        Command::Pull { dry_run } => println!("pull is not implemented yet; dry_run={dry_run}"),
    }

    Ok(())
}

fn handle_mission(command: MissionCommand, store: &LocalStore) -> Result<()> {
    match command {
        MissionCommand::Create { title } => {
            let mut event = TraceEvent::new(
                EventType::MissionCreated,
                default_actor(),
                json!({ "title": title }),
            );
            event.mission_id = Some(format!("mission-{}", event.event_id));
            let path = store.append_event(&event)?;
            println!("Recorded mission event in {}", path.display());
        }
    }
    Ok(())
}

fn handle_session(command: SessionCommand, store: &LocalStore) -> Result<()> {
    match command {
        SessionCommand::Start { mission } => {
            let mut event = TraceEvent::new(
                EventType::SessionStarted,
                default_actor(),
                json!({ "mission_id": mission }),
            );
            event.mission_id = mission;
            event.session_id = Some(format!("session-{}", event.event_id));
            let path = store.append_event(&event)?;
            println!("Recorded session event in {}", path.display());
        }
    }
    Ok(())
}

fn handle_artifact(command: ArtifactCommand, store: &LocalStore) -> Result<()> {
    match command {
        ArtifactCommand::Decision { mission, title } => {
            let mut event = TraceEvent::new(
                EventType::ArtifactCreated,
                default_actor(),
                json!({ "artifact_type": "decision", "title": title }),
            );
            event.mission_id = mission;
            event.artifact_id = Some(format!("artifact-{}", event.event_id));
            let path = store.append_event(&event)?;
            println!("Recorded artifact event in {}", path.display());
        }
    }
    Ok(())
}

fn default_actor() -> ActorRef {
    let actor_id = std::env::var("ORGII_TRACE_ACTOR_ID")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());

    ActorRef {
        actor_type: ActorType::Human,
        actor_id,
        display_name: None,
    }
}
