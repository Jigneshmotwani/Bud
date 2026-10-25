use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use budwood_cache::{serve, Cache, Config, DEFAULT_LISTEN};
use clap::Parser;

#[derive(Parser)]
#[command(name = "budwood-cache", version, about = "Budwood regional read-through object cache")]
struct Args {
    /// The next tier up: a budwood-server or another budwood-cache.
    #[arg(long, env = "BUDWOOD_UPSTREAM")]
    upstream: String,
    /// Address to serve the v1 API on.
    #[arg(long, env = "BUDWOOD_CACHE_LISTEN", default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// Directory holding cached objects.
    #[arg(long, env = "BUDWOOD_CACHE_DIR", default_value = "budwood-cache")]
    cache_dir: PathBuf,
    /// Size budget for cached objects, in bytes or with a K/M/G/T suffix
    /// (powers of 1024). Unbounded if omitted.
    #[arg(long, env = "BUDWOOD_MAX_CACHE_BYTES", value_parser = parse_size)]
    max_cache_bytes: Option<u64>,
}

fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let (digits, shift) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let shift = match c.to_ascii_uppercase() {
                'K' => 10,
                'M' => 20,
                'G' => 30,
                'T' => 40,
                _ => bail!("unknown size suffix {c:?}"),
            };
            (&s[..i], shift)
        }
        _ => (s, 0),
    };
    let n: u64 = digits.trim().parse().with_context(|| format!("invalid size {s:?}"))?;
    n.checked_mul(1 << shift).with_context(|| format!("size {s:?} is too large"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "budwood_cache=info".into()),
        )
        .init();
    let args = Args::parse();
    let config = Config { upstream: args.upstream, cache_dir: args.cache_dir, max_cache_bytes: args.max_cache_bytes };
    let cache = Arc::new(tokio::task::block_in_place(|| Cache::open(&config))?);
    let stats = cache.stats();
    tracing::info!(objects = stats.objects_cached, bytes = stats.bytes_cached, "opened {}", config.cache_dir.display());

    let listener =
        tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("binding {}", args.listen))?;
    tracing::info!("listening on http://{}, upstream {}", listener.local_addr()?, cache.upstream().base());
    serve(listener, cache, async {
        tokio::signal::ctrl_c().await.ok();
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("4k").unwrap(), 4096);
        assert_eq!(parse_size("10G").unwrap(), 10 << 30);
        assert!(parse_size("1X").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("99999999T").is_err());
    }
}
