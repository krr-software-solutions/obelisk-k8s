use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;
use obelisk_k8s::ObeliskK8sError;
use obelisk_k8s::config::global::{GLOBAL_ROOT_ENV, GlobalConfig, default_global_root};
use obelisk_k8s::config::repo::RepoConfig;

//

/// Kubernetes deploy tooling, for prod/dev and local k3d clusters.
#[derive(Debug, Parser)]
#[command(name = "obelisk-k8s", version)]
struct Cli {
    /// The machine-wide config directory [default: ~/.obelisk-k8s]
    #[arg(long, global = true, help_heading = SPECIAL, env = GLOBAL_ROOT_ENV, value_name = "DIR")]
    global_root: Option<PathBuf>,

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
    /// The machine-wide config (`~/.obelisk-k8s/`, or `--global-root`)
    Global {
        #[command(subcommand)]
        command: GlobalCommand,
    },
    /// The repo's own config (`.obelisk-k8s/`, in the current directory)
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum GlobalCommand {
    /// Load and check the global config, then print its repos and clusters
    Show,
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
        #[arg(long, help_heading = EXTRA)]
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

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn run_config_global_show(global_root: Option<PathBuf>) -> Result<(), ObeliskK8sError> {
    let home = home();
    let root = global_root
        .map(|root| expand_tilde(root, home.as_deref()))
        .or_else(|| default_global_root(home.as_deref()))
        .ok_or_else(|| {
            ObeliskK8sError::Usage(format!(
                "no home directory for the default global root: set `--global-root` or `{GLOBAL_ROOT_ENV}`"
            ))
        })?;
    let config = GlobalConfig::load(&root, home.as_deref())?;
    for warning in &config.warnings {
        eprintln!("WARN - {warning}");
    }
    print!("{config}");
    Ok(())
}

/// A leading `~/`, which the shell leaves alone in `--global-root=~/…` and in env vars.
fn expand_tilde(path: PathBuf, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path,
    }
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

// --help groups options under three headings. Only the exceptions are labelled where they're
// declared (e.g. `--explain`); `command()` files everything else.

/// What this command is about.
const CORE: &str = "Core Options";
/// Presentation and tweaks.
const EXTRA: &str = "Extra Options";
/// Plumbing present on every command.
const SPECIAL: &str = "Special Options";

/// The CLI definition with headings applied: anything unlabelled is Core, and the built-in
/// `--help` / `--version` are Special.
fn command() -> clap::Command {
    fn apply(cmd: clap::Command) -> clap::Command {
        cmd.mut_args(|arg| {
            if arg.get_help_heading().is_some() {
                arg
            } else if matches!(arg.get_id().as_str(), "help" | "version") {
                arg.help_heading(SPECIAL)
            } else {
                arg.help_heading(CORE)
            }
        })
        .mut_subcommands(apply)
    }
    // Built first so the auto-generated --help/--version exist for `apply` to move.
    let mut cmd = Cli::command();
    cmd.build();
    apply(cmd)
}

const BIN: &str = "obelisk-k8s";

fn completions(shell: Shell) -> String {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command(), BIN, &mut script);
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
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    match cli.command {
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Show,
                },
        } => run_config_global_show(cli.global_root),
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
        command().debug_assert();
    }

    #[test]
    fn expands_a_leading_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(
            expand_tilde("~/x".into(), Some(home)),
            Path::new("/home/me/x")
        );
        assert_eq!(expand_tilde("~".into(), Some(home)), Path::new("/home/me"));
        assert_eq!(expand_tilde("~x/y".into(), Some(home)), Path::new("~x/y"));
        assert_eq!(
            expand_tilde("/a/~/b".into(), Some(home)),
            Path::new("/a/~/b")
        );
        assert_eq!(expand_tilde("~/x".into(), None), Path::new("~/x"));
    }

    #[test]
    fn every_option_has_a_heading() {
        fn check(cmd: &clap::Command, path: &str) {
            for arg in cmd.get_arguments().filter(|a| !a.is_positional()) {
                let heading = arg.get_help_heading();
                assert!(
                    matches!(heading, Some(CORE | EXTRA | SPECIAL)),
                    "{path} --{}: {heading:?}",
                    arg.get_id()
                );
            }
            for sub in cmd.get_subcommands() {
                check(sub, &format!("{path} {}", sub.get_name()));
            }
        }
        check(&command(), BIN);
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
