use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use budwood_server::{serve, Service, DEFAULT_MAX_BODY_BYTES};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "budwood-server", version, about = "Budwood source control server")]
struct Args {
    /// Directory holding the object store and metadata database.
    #[arg(long, global = true, env = "BUDWOOD_DATA", default_value = "budwood-data")]
    data: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API.
    Serve {
        #[arg(long, env = "BUDWOOD_LISTEN", default_value = "127.0.0.1:7171")]
        listen: SocketAddr,
        /// Create these repositories at startup if they don't exist.
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Largest accepted request body, in bytes; bounds blob size.
        #[arg(long, default_value_t = DEFAULT_MAX_BODY_BYTES)]
        max_body_bytes: usize,
    },
    /// Create a repository.
    CreateRepo { name: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "budwood_server=info".into()),
        )
        .init();
    let args = Args::parse();
    let service = Arc::new(Service::open(&args.data)?);

    match args.command {
        Command::CreateRepo { name } => {
            let created = service.create_repo(&name)?;
            println!("{} repository {name}", if created { "created" } else { "existing" });
        }
        Command::Serve { listen, repos, max_body_bytes } => {
            for name in &repos {
                service.create_repo(name)?;
            }
            let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
            tracing::info!("listening on http://{}", listener.local_addr()?);
            serve(listener, service, max_body_bytes, async {
                tokio::signal::ctrl_c().await.ok();
            })
            .await?;
        }
    }
    Ok(())
}
