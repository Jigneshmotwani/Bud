use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use budwood_client::{Remote, Workspace};
use budwood_core::{Changeset, EntryKind, RepoPath};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bud", version, about = "Budwood: source control for always-online monorepos")]
struct Cli {
    /// Server URL. With --repo, read-only commands work without a workspace.
    #[arg(long, global = true, env = "BUD_SERVER")]
    server: Option<String>,
    #[arg(long, global = true, env = "BUD_REPO")]
    repo: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a repository on the server given by --server.
    CreateRepo { name: String },
    /// List the server's repositories.
    Repos,
    /// Create a working copy, optionally of only some paths.
    Clone {
        server: String,
        repo: String,
        dir: Option<PathBuf>,
        /// Check out only this path (repeatable). Default: everything.
        #[arg(long = "sparse", value_name = "PATH")]
        sparse: Vec<String>,
    },
    /// Show local changes.
    Status,
    /// Show local changes as a unified diff.
    Diff { paths: Vec<String> },
    /// Commit local changes (all, or only those under PATHs).
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(long, env = "BUD_AUTHOR")]
        author: Option<String>,
        paths: Vec<String>,
    },
    /// Update the working copy to the latest (or a given) revision.
    Sync {
        #[arg(long)]
        rev: Option<String>,
    },
    /// Discard local changes under PATHs.
    Revert {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Show history, optionally of one path.
    Log {
        path: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Only changesets older than this sequence number (for paging).
        #[arg(long, value_name = "SEQ")]
        before: Option<u64>,
    },
    /// Show one changeset.
    Show { rev: String },
    /// Print a file at a revision.
    Cat {
        path: String,
        #[arg(long, default_value = "head")]
        rev: String,
    },
    /// List a directory at a revision.
    Ls {
        path: Option<String>,
        #[arg(long, default_value = "head")]
        rev: String,
    },
    /// Show which changeset last changed each line of a file.
    Blame {
        path: String,
        #[arg(long, default_value = "head")]
        rev: String,
    },
    /// Commit, on the server, a changeset that undoes revision REV.
    Backout {
        rev: String,
        /// Message for the new changeset (default: Revert "<summary>" ...).
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long, env = "BUD_AUTHOR")]
        author: Option<String>,
    },
    /// List the files that differ between two revisions on the server.
    RemoteDiff {
        from: String,
        #[arg(default_value = "head")]
        to: String,
        path: Option<String>,
    },
    /// Manage which paths are checked out.
    Sparse {
        #[command(subcommand)]
        command: SparseCommand,
    },
}

#[derive(Subcommand)]
enum SparseCommand {
    /// Check out an additional path.
    Add { path: String },
    /// List checked-out paths.
    List,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Where read-only commands get their data: an explicit --server/--repo, or
/// the current workspace's remote.
struct ReadContext {
    remote: Remote,
    workspace: Option<Workspace>,
    cwd: PathBuf,
}

impl ReadContext {
    fn new(cli: &Cli) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        if let (Some(server), Some(repo)) = (&cli.server, &cli.repo) {
            return Ok(Self { remote: Remote::new(server, repo)?, workspace: None, cwd });
        }
        let workspace = Workspace::open(&cwd)?;
        Ok(Self { remote: workspace.remote().clone(), workspace: Some(workspace), cwd })
    }

    /// Inside a workspace, paths are relative to the current directory;
    /// otherwise they are repository paths.
    fn path(&self, arg: Option<&str>) -> Result<RepoPath> {
        match (&self.workspace, arg) {
            (Some(ws), Some(arg)) => ws.repo_path(&self.cwd, arg),
            (Some(ws), None) => ws.repo_path(&self.cwd, "."),
            (None, arg) => Ok(RepoPath::new(arg.unwrap_or(""))?),
        }
    }
}

fn open_workspace() -> Result<(Workspace, PathBuf)> {
    let cwd = std::env::current_dir()?;
    Ok((Workspace::open(&cwd)?, cwd))
}

fn paths(ws: &Workspace, cwd: &Path, args: &[String]) -> Result<Vec<RepoPath>> {
    args.iter().map(|a| ws.repo_path(cwd, a)).collect()
}

fn run(cli: Cli) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match &cli.command {
        Command::CreateRepo { name } => {
            let server = cli.server.as_deref().ok_or_else(|| anyhow!("--server is required"))?;
            let created = Remote::create_repo(server, name)?;
            writeln!(out, "{} {name}", if created { "created" } else { "already exists:" })?;
        }
        Command::Repos => {
            let server = cli.server.as_deref().ok_or_else(|| anyhow!("--server is required"))?;
            for name in Remote::list_repos(server)? {
                writeln!(out, "{name}")?;
            }
        }
        Command::Clone { server, repo, dir, sparse } => {
            let dir = dir.clone().unwrap_or_else(|| PathBuf::from(repo));
            let sparse = sparse.iter().map(|p| RepoPath::new(p.as_str())).collect::<Result<Vec<_>, _>>()?;
            let ws = Workspace::clone(server, repo, &dir, sparse)?;
            writeln!(out, "cloned {repo} at revision {} into {}", ws.base(), dir.display())?;
        }
        Command::Status => {
            let (mut ws, _) = open_workspace()?;
            let changes = ws.status()?;
            writeln!(
                out,
                "{} at revision {} ({})",
                ws.config().repo,
                ws.base(),
                ws.config().sparse.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            )?;
            if changes.is_empty() {
                writeln!(out, "no local changes")?;
            }
            for change in changes {
                writeln!(out, "{} {}", change.kind.letter(), change.path)?;
            }
        }
        Command::Diff { paths: args } => {
            let (mut ws, cwd) = open_workspace()?;
            let only = paths(&ws, &cwd, args)?;
            write!(out, "{}", ws.diff(&only)?)?;
        }
        Command::Commit { message, author, paths: args } => {
            let (mut ws, cwd) = open_workspace()?;
            let only = paths(&ws, &cwd, args)?;
            let author = author
                .clone()
                .or_else(|| std::env::var("USER").ok())
                .ok_or_else(|| anyhow!("set --author or BUD_AUTHOR"))?;
            let outcome = ws.commit(message, &author, &only)?;
            let cs = &outcome.changeset;
            write!(out, "committed revision {} ({}), {} file(s)", cs.seq, cs.id.short(), cs.changes.len())?;
            if outcome.rebased {
                write!(out, "; rebased over newer changes, {} file(s) updated locally", outcome.pulled)?;
            }
            writeln!(out)?;
        }
        Command::Sync { rev } => {
            let (mut ws, _) = open_workspace()?;
            let outcome = ws.sync(rev.as_deref())?;
            if outcome.from == outcome.to {
                writeln!(out, "already at revision {}", outcome.to)?;
            } else {
                writeln!(out, "updated {} -> {}, {} file(s) changed", outcome.from, outcome.to, outcome.updated)?;
            }
        }
        Command::Revert { paths: args } => {
            let (mut ws, cwd) = open_workspace()?;
            let only = paths(&ws, &cwd, args)?;
            for change in ws.revert(&only)? {
                writeln!(out, "reverted {}", change.path)?;
            }
        }
        Command::Log { path, limit, before } => {
            let ctx = ReadContext::new(&cli)?;
            let path = ctx.path(path.as_deref())?;
            for cs in ctx.remote.log(&path, *before, *limit)? {
                writeln!(out, "{}", log_line(&cs))?;
            }
        }
        Command::Show { rev } => {
            let ctx = ReadContext::new(&cli)?;
            let cs = ctx.remote.changeset(rev)?;
            writeln!(out, "revision {}  {}", cs.seq, cs.id)?;
            writeln!(out, "author   {}", cs.author)?;
            writeln!(out, "date     {}", format_time(cs.timestamp))?;
            if cs.base + 1 < cs.seq {
                writeln!(out, "base     {} (rebased by the server)", cs.base)?;
            }
            writeln!(out)?;
            for line in cs.message.lines() {
                writeln!(out, "    {line}")?;
            }
            writeln!(out)?;
            for change in &cs.changes {
                writeln!(out, "{} {}", if change.new.is_some() { '+' } else { '-' }, change.path)?;
            }
        }
        Command::Cat { path, rev } => {
            let ctx = ReadContext::new(&cli)?;
            let path = ctx.path(Some(path))?;
            out.write_all(&ctx.remote.file(rev, &path)?)?;
        }
        Command::Ls { path, rev } => {
            let ctx = ReadContext::new(&cli)?;
            let path = ctx.path(path.as_deref())?;
            let listing = ctx.remote.tree(rev, &path)?;
            if listing.kind != EntryKind::Tree {
                writeln!(out, "{}", listing.path)?;
            }
            for entry in listing.entries {
                let suffix = match entry.kind {
                    EntryKind::Tree => "/",
                    EntryKind::Executable => "*",
                    EntryKind::Symlink => "@",
                    EntryKind::File => "",
                };
                writeln!(out, "{}{suffix}", entry.name)?;
            }
        }
        Command::Blame { path, rev } => {
            let ctx = ReadContext::new(&cli)?;
            let path = ctx.path(Some(path))?;
            let blame = ctx.remote.blame(rev, &path)?;
            let width = blame.lines.last().map_or(1, |l| l.line_no.to_string().len());
            for line in &blame.lines {
                writeln!(
                    out,
                    "{:>6} {} {:<12} {} {:>width$}| {}",
                    line.seq,
                    &line.id.to_hex()[..12],
                    truncate(&line.author, 12),
                    &format_time(line.timestamp)[..10],
                    line.line_no,
                    line.text
                )?;
            }
        }
        Command::Backout { rev, message, author } => {
            let ctx = ReadContext::new(&cli)?;
            let author = author
                .clone()
                .or_else(|| std::env::var("USER").ok())
                .ok_or_else(|| anyhow!("set --author or BUD_AUTHOR"))?;
            let resp = ctx.remote.revert(rev, &author, message.as_deref())?;
            let cs = &resp.changeset;
            writeln!(
                out,
                "committed revision {} ({}), backing out revision {}; {} file(s)",
                cs.seq,
                cs.id.short(),
                cs.base,
                cs.changes.len()
            )?;
            if ctx.workspace.is_some() {
                writeln!(out, "run `bud sync` to update the working copy")?;
            }
        }
        Command::RemoteDiff { from, to, path } => {
            let ctx = ReadContext::new(&cli)?;
            let path = ctx.path(path.as_deref())?;
            for change in ctx.remote.diff(from, to, &path)?.changes {
                writeln!(out, "{} {}", if change.new.is_some() { '+' } else { '-' }, change.path)?;
            }
        }
        Command::Sparse { command } => {
            let (mut ws, cwd) = open_workspace()?;
            match command {
                SparseCommand::Add { path } => {
                    let path = ws.repo_path(&cwd, path)?;
                    let count = ws.sparse_add(path.clone())?;
                    writeln!(out, "checked out {count} file(s) under {path}")?;
                }
                SparseCommand::List => {
                    for path in &ws.config().sparse {
                        writeln!(out, "{path}")?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn log_line(cs: &Changeset) -> String {
    format!(
        "{:>6}  {}  {:<12}  {}  {}",
        cs.seq,
        cs.id.short(),
        truncate(&cs.author, 12),
        format_time(cs.timestamp),
        cs.summary()
    )
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `YYYY-MM-DD HH:MM` in UTC.
fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days, after Howard Hinnant.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_793_000_000), "2026-10-26 07:33");
    }
}
