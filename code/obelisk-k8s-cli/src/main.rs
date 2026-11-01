use std::process::ExitCode;

use clap::{Parser, Subcommand};
use obelisk_k8s::{ObeliskK8sError, hello};

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
    /// Print a greeting
    Hello {
        /// Who to greet
        #[arg(long, default_value = "world")]
        name: String,
    },
}

//

fn run_hello(name: &str) -> Result<(), ObeliskK8sError> {
    println!("{}", hello::greet(name)?);
    Ok(())
}

fn main_impl() -> Result<(), ObeliskK8sError> {
    match Cli::parse().command {
        Command::Hello { name } => run_hello(&name),
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
