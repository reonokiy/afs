//! git-remote-afs: Git remote helper for afs:// URLs.
//!
//! Stores git objects and refs in S3 (or any opendal backend).
//!
//! URL format:
//!   afs://<path-or-prefix>
//!
//! Configure backend via AFS_BACKEND_CONFIG env var pointing to a TOML file,
//! or use a local FS path directly: afs:///tmp/my-remote
//!
//! S3 layout:
//!   refs.json              — ref → oid mapping
//!   git/pack-<hash>.pack   — git pack files

mod refs;
pub mod transport;

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use tracing::debug;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Support "git-remote-afs gc <url>" as a direct subcommand
    if args.len() >= 3 && args[1] == "gc" {
        return run_gc(&args[2]);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(io::stderr)
        .init();

    if args.len() < 3 {
        anyhow::bail!("usage: git-remote-afs <remote> <url>\n       git-remote-afs gc <url>");
    }

    let url = &args[2];
    debug!(%url, "git-remote-afs starting");

    let rt = tokio::runtime::Runtime::new()?;
    let remote = rt.block_on(transport::Remote::from_url(url))?;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut shallow_depth: Option<u32> = None;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();

        if line.is_empty() {
            continue;
        }

        debug!(cmd = %line, "command");

        if line == "capabilities" {
            writeln!(out, "push")?;
            writeln!(out, "fetch")?;
            writeln!(out, "option")?;
            writeln!(out)?;
            out.flush()?;
        } else if line == "list" || line == "list for-push" {
            let remote_refs = rt.block_on(remote.list_refs())?;
            for (refname, oid) in &remote_refs {
                writeln!(out, "{} {}", oid, refname)?;
            }
            writeln!(out)?;
            out.flush()?;
        } else if let Some(rest) = line.strip_prefix("option ") {
            // Handle options like "option depth 1"
            if let Some(depth_str) = rest.strip_prefix("depth ") {
                if let Ok(d) = depth_str.parse::<u32>() {
                    shallow_depth = Some(d);
                    writeln!(out, "ok")?;
                } else {
                    writeln!(out, "error invalid depth")?;
                }
            } else {
                writeln!(out, "unsupported")?;
            }
            out.flush()?;
        } else if let Some(rest) = line.strip_prefix("push ") {
            let mut push_specs = vec![rest.to_string()];

            loop {
                let mut next = String::new();
                reader.read_line(&mut next)?;
                let next = next.trim().to_string();
                if next.is_empty() {
                    break;
                }
                if let Some(rest) = next.strip_prefix("push ") {
                    push_specs.push(rest.to_string());
                }
            }

            for spec in &push_specs {
                match rt.block_on(remote.push(spec)) {
                    Ok(()) => writeln!(out, "ok {}", spec_dst(spec))?,
                    Err(e) => writeln!(out, "error {} {}", spec_dst(spec), e)?,
                }
            }
            writeln!(out)?;
            out.flush()?;
        } else if let Some(rest) = line.strip_prefix("fetch ") {
            let mut fetch_specs = vec![rest.to_string()];

            loop {
                let mut next = String::new();
                reader.read_line(&mut next)?;
                let next = next.trim().to_string();
                if next.is_empty() {
                    break;
                }
                if let Some(rest) = next.strip_prefix("fetch ") {
                    fetch_specs.push(rest.to_string());
                }
            }

            for spec in &fetch_specs {
                if let Some(depth) = shallow_depth {
                    rt.block_on(remote.fetch_shallow(spec, depth))
                        .with_context(|| format!("shallow fetch {}", spec))?;
                } else {
                    rt.block_on(remote.fetch(spec))
                        .with_context(|| format!("fetch {}", spec))?;
                }
            }
            writeln!(out)?;
            out.flush()?;
        } else {
            writeln!(out)?;
            out.flush()?;
        }
    }

    Ok(())
}

fn run_gc(url: &str) -> Result<()> {
    // try_init because tracing may already be initialized
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let rt = tokio::runtime::Runtime::new()?;
    let remote = rt.block_on(transport::Remote::from_url(url))?;
    let stats = rt.block_on(remote.gc())?;

    println!(
        "GC complete: {} packs → {} pack ({} → {} bytes)",
        stats.packs_before, stats.packs_after, stats.bytes_before, stats.bytes_after
    );

    Ok(())
}

fn spec_dst(spec: &str) -> &str {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    match spec.split_once(':') {
        Some((_, dst)) => dst,
        None => spec,
    }
}
