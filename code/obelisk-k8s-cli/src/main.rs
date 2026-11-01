use std::process::ExitCode;

use clap::{Parser, Subcommand};
use obelisk_k8s::ObeliskK8sError;
use obelisk_k8s::config::repo::RepoConfig;

//

/// Kubernetes deploy tooling, for prod/dev and local k3d clusters.
#[derive(Debug, Parser)]
#[command(name = "obelisk-k8s", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read and check config files
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// The repo's own config (`.obelisk-k8s/`, in the current directory)
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Load and check the repo's config, then print its targets and units
    Show,
}

//

fn run_config_repo_show() -> Result<(), ObeliskK8sError> {
    let cwd = std::env::current_dir().map_err(|source| ObeliskK8sError::Io {
        path: ".".into(),
        source,
    })?;
    let config = RepoConfig::load(&cwd)?;
    for warning in &config.warnings {
        eprintln!("WARN - {warning}");
    }
    print!("{config}");
    Ok(())
}

fn main_impl() -> Result<(), ObeliskK8sError> {
    match Cli::parse().command {
        Command::Config {
            command: ConfigCommand::Repo {
                command: RepoCommand::Show,
            },
        } => run_config_repo_show(),
    }
}

fn main() -> ExitCode {
    match main_impl() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR - {error}");
            ExitCode::FAILURE
        }
    }
}
