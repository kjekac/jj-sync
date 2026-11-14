use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jj_sync::{Config, Sync};
use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Publish workspace tips and fetch peer repositories through Jujutsu"
)]
struct Args {
    #[arg(long, default_value = "/etc/jj-sync.json", global = true)]
    config: PathBuf,
    #[arg(long, global = true, default_value = "auto", value_parser = ["auto", "always", "never"])]
    color: String,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Cycle,
    /// Publish local operation changes and refresh staging on peer notifications.
    Watch {
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..))]
        debounce_ms: u64,
    },
    /// Publish workspace tips and queue notifications without fetching peers.
    Publish,
    Serve,
    Setup,
    PublicKey,
    Pair {
        peer: String,
    },
    PairReceive {
        peer: String,
    },
    Revoke {
        peer: String,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Pending {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Cached numeric output for shell prompts; never refreshes.
        #[arg(long, conflicts_with = "json")]
        count: bool,
        #[arg(long)]
        json: bool,
        /// Inspect the last downloaded snapshots without contacting peers.
        #[arg(long)]
        cached: bool,
    },
    Apply {
        #[arg(long)]
        json: bool,
        #[arg(long = "dry-run", alias = "preview")]
        dry_run: bool,
        #[arg(long)]
        cached: bool,
        /// Include the full native JJ operation diff.
        #[arg(long)]
        verbose: bool,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    Github,
}
fn execute(args: Args) -> Result<()> {
    let sync = Sync::new(Config::from_json(
        &fs::read_to_string(&args.config)
            .with_context(|| format!("Reading {}", args.config.display()))?,
    )?);
    if matches!(args.command, Action::Serve) {
        return sync.serve(&std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default());
    }
    if let Action::Watch {
        interval,
        debounce_ms,
    } = &args.command
    {
        return sync.watch(
            std::time::Duration::from_secs(*interval),
            std::time::Duration::from_millis(*debounce_ms),
        );
    }
    if matches!(args.command, Action::Publish) {
        sync.publish()?;
        return Ok(());
    }
    if let Action::Pending {
        path, count: true, ..
    } = &args.command
    {
        println!("{}", sync.pending(path)?);
        return Ok(());
    }
    let _lock = sync.lock()?;
    match args.command {
        Action::Cycle => sync.cycle(),
        Action::Setup => sync.setup(),
        Action::PublicKey => {
            println!("{}", sync.public_key()?);
            Ok(())
        }
        Action::Pair { peer } => sync.pair(&peer),
        Action::PairReceive { peer } => {
            let mut input = String::new();
            io::stdin().take(4097).read_to_string(&mut input)?;
            anyhow::ensure!(input.len() <= 4096, "Public key input exceeds 4096 bytes");
            sync.accept_key(&peer, input.trim())?;
            println!("{}", sync.public_key()?);
            Ok(())
        }
        Action::Revoke { peer } => sync.revoke(&peer),
        Action::Github => sync.github(),
        Action::Apply {
            path,
            dry_run,
            cached,
            verbose,
            json,
        } => {
            if !cached && (dry_run || !sync.has_preview(&path)?) {
                sync.cycle()?;
            }
            sync.apply_with_output(&path, dry_run, verbose, json, &args.color)
        }
        Action::Pending {
            path, json, cached, ..
        } => {
            if !cached {
                if let Err(error) = sync.cycle() {
                    eprintln!("Refresh failed; showing cached results: {error:#}");
                }
            }
            sync.show_pending_with_color(&path, json, &args.color)
        }
        Action::Status { json } => {
            let path = sync.config.state.join("status.json");
            if path.exists() {
                print!("{}", fs::read_to_string(path)?);
            } else if json {
                println!("{{}}");
            } else {
                println!("No sync cycle has run yet.");
            }
            Ok(())
        }
        Action::Serve | Action::Watch { .. } | Action::Publish => unreachable!(),
    }
}
fn main() -> ExitCode {
    let _signals = match jj_sync::install_signal_handlers() {
        Ok(handlers) => handlers,
        Err(error) => {
            eprintln!("jj-sync: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let start = std::time::Instant::now();
    let args = Args::parse();
    let cycle = matches!(args.command, Action::Cycle);
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jj-sync: {error:#}");
            let signal = jj_sync::received_signal();
            if signal != 0 {
                return ExitCode::from((128 + signal) as u8);
            }
            // A denied network operation can fail before macOS displays its alert.
            // Retain the native responsible process long enough to show it.
            #[cfg(target_os = "macos")]
            if cycle {
                while start.elapsed() < std::time::Duration::from_secs(30) {
                    let signal = jj_sync::received_signal();
                    if signal != 0 {
                        return ExitCode::from((128 + signal) as u8);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (start, cycle);
            ExitCode::from(1)
        }
    }
}
