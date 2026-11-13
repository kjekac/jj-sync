use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jj_repo_sync::{Config, Sync};
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
    #[arg(long, default_value = "/etc/jj-repo-sync.json", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Cycle,
    Serve,
    Setup,
    PublicKey,
    Pair { peer: String },
    PairReceive { peer: String },
    Revoke { peer: String },
    Status,
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
        Action::Status => {
            let path = sync.config.state.join("status.json");
            if path.exists() {
                print!("{}", fs::read_to_string(path)?);
            } else {
                println!("No sync cycle has run yet.");
            }
            Ok(())
        }
        Action::Serve => unreachable!(),
    }
}
fn main() -> ExitCode {
    let _signals = match jj_repo_sync::install_signal_handlers() {
        Ok(handlers) => handlers,
        Err(error) => {
            eprintln!("jj-repo-sync: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let start = std::time::Instant::now();
    let args = Args::parse();
    let cycle = matches!(args.command, Action::Cycle);
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jj-repo-sync: {error:#}");
            let signal = jj_repo_sync::received_signal();
            if signal != 0 {
                return ExitCode::from((128 + signal) as u8);
            }
            // A denied network operation can fail before macOS displays its alert.
            // Retain the native responsible process long enough to show it.
            #[cfg(target_os = "macos")]
            if cycle {
                while start.elapsed() < std::time::Duration::from_secs(30) {
                    let signal = jj_repo_sync::received_signal();
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
