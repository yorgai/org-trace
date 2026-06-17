use anyhow::Result;
use axum::{routing::get, Json, Router};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(name = "orgii-trace-server")]
#[command(about = "Self-hosted ORGII Trace provenance server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:7821")]
        bind: String,
    },
    Migrate,
    CreateAdmin,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { bind } => serve(bind).await?,
        Command::Migrate => println!("migrate is not implemented yet"),
        Command::CreateAdmin => println!("create-admin is not implemented yet"),
    }

    Ok(())
}

async fn serve(bind: String) -> Result<()> {
    let app = Router::new().route("/health", get(health));
    let listener = TcpListener::bind(&bind).await?;
    println!("orgii-trace-server listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}
