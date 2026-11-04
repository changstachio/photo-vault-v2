mod app;
mod archive;
mod config;
mod dates;
mod ingest;
mod job;
mod preview;
mod shrink;
mod sync;
mod util;
mod web;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "vault", about = "Photo backup, ingest, archive and date sorting", version)]
struct Cli {
    /// Config file (default: $VAULT_CONFIG, ~/.config/photovault/config.toml, /etc/photovault/config.toml)
    #[arg(long, short)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write an example config (refuses to overwrite)
    Init { path: Option<PathBuf> },
    /// Check the config file
    Check,
    /// 1: copy the configured folders to the network share
    Sync { #[arg(long)] dry_run: bool },
    /// 2: copy new projects from the ingest cache into the local backup, then free the cache
    Ingest { #[arg(long)] dry_run: bool },
    /// 3: convert ARW to JPG and zip projects with no ARW left, oldest first
    Archive {
        #[arg(long)] dry_run: bool,
        #[arg(long)] limit: Option<usize>,
        /// ARW with a same-name JPG: rename | use_existing | overwrite (default: config)
        #[arg(long)] existing_jpg: Option<config::ExistingJpg>,
    },
    /// 5: re-encode JPGs in the folders/zips chosen as "shrink" at the configured quality
    Shrink { #[arg(long)] dry_run: bool },
    /// 4: report (or with --apply, fix) projects sitting in the wrong year folder
    Dates { #[arg(long)] apply: bool },
    /// Run the scheduler and web UI (for systemd)
    Daemon,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Init { path } = &cli.cmd {
        let p = path.clone().or(cli.config.clone()).unwrap_or_else(|| "photovault.toml".into());
        if p.exists() {
            bail!("{} exists", p.display());
        }
        std::fs::write(&p, config::EXAMPLE)?;
        println!("wrote {}", p.display());
        return Ok(());
    }
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    let cfg = config::Config::load(&path)?;
    let cli_ctx = |dry, apply| job::Ctx::new(dry, apply, true);
    match cli.cmd {
        Cmd::Init { .. } => unreachable!(),
        Cmd::Check => println!("{} is valid", path.display()),
        Cmd::Sync { dry_run } => app::run_task(&cfg, "sync", &cli_ctx(dry_run, false), None)?,
        Cmd::Ingest { dry_run } => app::run_task(&cfg, "ingest", &cli_ctx(dry_run, false), None)?,
        Cmd::Archive { dry_run, limit, existing_jpg } => {
            let ctx = cli_ctx(dry_run, false);
            *ctx.existing.lock().unwrap() = existing_jpg;
            app::run_task(&cfg, "archive", &ctx, limit)?
        }
        Cmd::Shrink { dry_run } => app::run_task(&cfg, "shrink", &cli_ctx(dry_run, false), None)?,
        Cmd::Dates { apply } => app::run_task(&cfg, "dates", &cli_ctx(false, apply), None)?,
        Cmd::Daemon => {
            let a = app::App::new(cfg, path.clone());
            let s = a.clone();
            std::thread::spawn(move || app::scheduler(s));
            if a.cfg().web.enabled {
                web::serve(a).context("web server")?;
            } else {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
