use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
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
    /// Print a shell completion script, e.g. `source <(obelisk-k8s completions bash)`
    Completions {
        /// The shell to complete for
        shell: Shell,
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
    /// Print the config files each unit gets on each target, in merge order (lowest priority first)
    Paths {
        /// Only these targets (repeatable; default: all)
        #[arg(short, long = "target", value_name = "TARGET")]
        targets: Vec<String>,
        /// Only these units (repeatable; default: all)
        #[arg(short, long = "unit", value_name = "UNIT")]
        units: Vec<String>,
        /// List every config path, including skipped ones (no such file), as a table
        #[arg(long)]
        explain: bool,
    },
}

//

fn cwd() -> Result<PathBuf, ObeliskK8sError> {
    std::env::current_dir().map_err(|source| ObeliskK8sError::Io {
        path: ".".into(),
        source,
    })
}

fn load_repo_config(repo_root: &Path) -> Result<RepoConfig, ObeliskK8sError> {
    let config = RepoConfig::load(repo_root)?;
    for warning in &config.warnings {
        eprintln!("WARN - {warning}");
    }
    Ok(config)
}

fn run_config_repo_show() -> Result<(), ObeliskK8sError> {
    print!("{}", load_repo_config(&cwd()?)?);
    Ok(())
}

fn run_config_repo_paths(
    targets: &[String],
    units: &[String],
    explain: bool,
) -> Result<(), ObeliskK8sError> {
    let repo_root = cwd()?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(targets, units)?;
    // anstream drops the colors when stdout isn't a terminal, or with NO_COLOR set.
    anstream::print!("{}", config.paths_report(&repo_root, &selection, explain)?);
    Ok(())
}

const BIN: &str = "obelisk-k8s";

fn completions(shell: Shell) -> String {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), BIN, &mut script);
    let script = String::from_utf8_lossy(&script).into_owned();
    if shell == Shell::Bash {
        // clap_complete 4.6.11 mangles a hyphenated binary name two ways in the bash script: the
        // word loop sets `cmd="obelisk__k8s__subcmd__config"`, but the dispatch `case` labels it
        // `obelisk__subcmd__k8s__subcmd__config)`, so no subcommand ever completes. Make them agree.
        script.replace(&BIN.replace('-', "__subcmd__"), &BIN.replace('-', "__"))
    } else {
        script
    }
}

fn run_completions(shell: Shell) {
    print!("{}", completions(shell));
}

fn main_impl() -> Result<(), ObeliskK8sError> {
    match Cli::parse().command {
        Command::Config {
            command: ConfigCommand::Repo {
                command: RepoCommand::Show,
            },
        } => run_config_repo_show(),
        Command::Config {
            command:
                ConfigCommand::Repo {
                    command:
                        RepoCommand::Paths {
                            targets,
                            units,
                            explain,
                        },
                },
        } => run_config_repo_paths(&targets, &units, explain),
        Command::Completions { shell } => {
            run_completions(shell);
            Ok(())
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bash_dispatches_every_command_it_recognizes() {
        let script = completions(Shell::Bash);
        let assigned: Vec<&str> = script
            .lines()
            .filter_map(|l| l.trim().strip_prefix("cmd=\"")?.strip_suffix('"'))
            .filter(|c| !c.is_empty())
            .collect();
        assert!(
            assigned.contains(&"obelisk__k8s__subcmd__config__subcmd__repo"),
            "{script}"
        );
        for cmd in assigned {
            assert!(
                script.contains(&format!("        {cmd})\n")),
                "no `{cmd})` case in:\n{script}"
            );
        }
    }
}
