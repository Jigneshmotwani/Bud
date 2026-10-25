//! `budwood-import`: import a git repository's first-parent history into Budwood.

use std::path::PathBuf;

use anyhow::Result;
use budwood_client::Remote;
use budwood_import::{import, Options};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "budwood-import",
    version,
    about = "Import a git repository's first-parent history into a Budwood repository",
    long_about = "Import a git repository's first-parent history into a Budwood repository.\n\n\
                  Each git commit becomes one changeset holding its diff against its first parent, with \
                  `Git-Commit:` and `Git-Date:` trailers. Running the import again continues after the last \
                  imported commit."
)]
struct Cli {
    /// Path to the local git repository.
    git_repo: PathBuf,
    /// The git revision to import.
    #[arg(default_value = "HEAD")]
    rev: String,
    /// Budwood server URL.
    #[arg(long, env = "BUD_SERVER")]
    server: String,
    /// Budwood repository name.
    #[arg(long, env = "BUD_REPO")]
    repo: String,
    /// Create the Budwood repository if it doesn't exist.
    #[arg(long)]
    create_repo: bool,
    /// Process at most N git commits in this run.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    /// Show what would be imported without uploading or committing.
    #[arg(long)]
    dry_run: bool,
    /// After importing, check that Budwood's head matches the git tree.
    #[arg(long)]
    verify: bool,
    /// Import even if the Budwood history doesn't continue the git history.
    #[arg(long)]
    force: bool,
    /// Parallel blob uploads.
    #[arg(long, default_value_t = 8, value_name = "N")]
    threads: usize,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let remote = Remote::new(&cli.server, &cli.repo)?;
    if cli.create_repo && !cli.dry_run && Remote::create_repo(&cli.server, &cli.repo)? {
        eprintln!("created repository {}", cli.repo);
    }
    let opts = Options {
        git_dir: cli.git_repo,
        rev: cli.rev,
        limit: cli.limit,
        force: cli.force,
        dry_run: cli.dry_run,
        verify: cli.verify,
        threads: cli.threads,
    };
    let summary = import(&remote, &opts)?;
    print!("{summary}");
    Ok(())
}
