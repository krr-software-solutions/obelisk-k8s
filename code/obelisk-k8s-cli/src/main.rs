use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use obelisk_k8s::ObeliskK8sError;
use obelisk_k8s::config::global::edit::NewCluster;
use obelisk_k8s::config::global::{GLOBAL_ROOT_ENV, K8s, Location, Obeliskd};

mod commands;

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
    /// Render what a unit deploys, step by step, without touching a cluster
    Template {
        /// The repo, by its name in the global `repos.yaml` [default: the one in the current directory]
        #[arg(long, global = true, help_heading = SPECIAL, value_name = "NAME")]
        repo: Option<String>,

        #[command(subcommand)]
        command: TemplateCommand,
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
    /// The repo's own config (`.obelisk-k8s/`, in the current directory or `--repo`)
    Repo {
        /// The repo, by its name in the global `repos.yaml` [default: the one in the current directory]
        #[arg(long, global = true, help_heading = SPECIAL, value_name = "NAME")]
        repo: Option<String>,

        #[command(subcommand)]
        command: RepoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum GlobalCommand {
    /// Load and check the global config, then print its repos and clusters
    Show,
    /// Create the global root, an empty `repos.yaml` and `clusters/` (whichever are missing)
    Init,
    /// Repos: the entries in `repos.yaml`
    Repo {
        #[command(subcommand)]
        command: GlobalRepoCommand,
    },
    /// Clusters: `clusters/<name>/cluster.yaml`
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
}

#[derive(Debug, Subcommand)]
enum GlobalRepoCommand {
    /// Add an entry to `repos.yaml`, as written (relative to the global root, `~/…`, or absolute;
    /// `dir/*` for every repo in a directory)
    Add {
        /// The entry
        path: String,
    },
    /// Remove an entry from `repos.yaml`: the one written exactly so, else the one listing that repo
    Remove {
        /// A repo's name, or an entry as written
        name_or_path: String,
    },
}

#[derive(Debug, Subcommand)]
enum ClusterCommand {
    /// Write a new `clusters/<NAME>/cluster.yaml`
    Add(ClusterAdd),
    /// Delete a cluster's directory (only if it holds nothing but its `cluster.yaml`)
    Remove {
        /// The cluster
        name: String,
    },
}

#[derive(Debug, Args)]
struct ClusterAdd {
    /// The cluster's name (also its directory's)
    name: String,
    /// What runs it
    #[arg(long, value_enum)]
    k8s: K8sArg,
    /// Where it runs
    #[arg(long, value_enum)]
    location: LocationArg,
    /// obelisk-k8s creates and destroys it (not acted on yet)
    #[arg(long)]
    managed: bool,
    /// The obeliskd supernet its addresses come from (managed only; needs `--obeliskd-subnet`)
    #[arg(long, requires_all = ["obeliskd_subnet", "managed"], value_name = "SUPERNET")]
    obeliskd_supernet: Option<String>,
    /// The obeliskd subnet its addresses come from (managed only; needs `--obeliskd-supernet`)
    #[arg(long, requires_all = ["obeliskd_supernet", "managed"], value_name = "SUBNET")]
    obeliskd_subnet: Option<String>,
    /// Its cluster in your kubeconfig
    #[arg(long, value_name = "CLUSTER")]
    kubectl_cluster: Option<String>,
    /// Its context in your kubeconfig
    #[arg(long, value_name = "CONTEXT")]
    kubectl_context: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum K8sArg {
    K3d,
    Talos,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LocationArg {
    Local,
    Remote,
}

impl From<ClusterAdd> for NewCluster {
    fn from(a: ClusterAdd) -> Self {
        Self {
            name: a.name,
            k8s: match a.k8s {
                K8sArg::K3d => K8s::K3d,
                K8sArg::Talos => K8s::Talos,
            },
            location: match a.location {
                LocationArg::Local => Location::Local,
                LocationArg::Remote => Location::Remote,
            },
            managed: a.managed,
            obeliskd: a
                .obeliskd_supernet
                .zip(a.obeliskd_subnet)
                .map(|(supernet, subnet)| Obeliskd { supernet, subnet }),
            kubectl_cluster: a.kubectl_cluster,
            kubectl_context: a.kubectl_context,
        }
    }
}

#[derive(Debug, Subcommand)]
enum TemplateCommand {
    /// The merged values each unit gets on each target: its config files, deep-merged in merge order,
    /// with the `obelisk:` block on top (SSM refs not resolved)
    Values {
        /// Only these targets (repeatable; default: all)
        #[arg(short, long = "target", value_name = "TARGET")]
        targets: Vec<String>,
        /// Only these units (repeatable; default: all)
        #[arg(short, long = "unit", value_name = "UNIT")]
        units: Vec<String>,
    },
    /// The values after each unit's generators: its merged values, with every generator's output
    /// deep-merged on top (generators are Go templates, rendered by `helm`)
    GeneratedValues {
        /// Only these targets (repeatable; default: all)
        #[arg(short, long = "target", value_name = "TARGET")]
        targets: Vec<String>,
        /// Only these units (repeatable; default: all)
        #[arg(short, long = "unit", value_name = "UNIT")]
        units: Vec<String>,
    },
    /// The k8s resources each unit renders on each target: its templates, rendered with its generated
    /// values and joined in merge order (templates are Go templates, rendered by `helm`)
    Manifests {
        /// Only these targets (repeatable; default: all)
        #[arg(short, long = "target", value_name = "TARGET")]
        targets: Vec<String>,
        /// Only these units (repeatable; default: all)
        #[arg(short, long = "unit", value_name = "UNIT")]
        units: Vec<String>,
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
        #[arg(long, help_heading = EXTRA)]
        explain: bool,
    },
}

//

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
        } => commands::run_config_global_show(cli.global_root),
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Init,
                },
        } => commands::run_config_global_init(cli.global_root),
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Cluster { command },
                },
        } => commands::run_config_global_cluster(cli.global_root, command),
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Repo { command },
                },
        } => commands::run_config_global_repo(cli.global_root, command),
        Command::Config {
            command:
                ConfigCommand::Repo {
                    repo,
                    command: RepoCommand::Show,
                },
        } => commands::run_config_repo_show(repo.as_deref(), cli.global_root),
        Command::Config {
            command:
                ConfigCommand::Repo {
                    repo,
                    command:
                        RepoCommand::Paths {
                            targets,
                            units,
                            explain,
                        },
                },
        } => commands::run_config_repo_paths(
            repo.as_deref(),
            cli.global_root,
            &targets,
            &units,
            explain,
        ),
        Command::Template {
            repo,
            command: TemplateCommand::Values { targets, units },
        } => commands::run_template_values(repo.as_deref(), cli.global_root, &targets, &units),
        Command::Template {
            repo,
            command: TemplateCommand::Manifests { targets, units },
        } => commands::run_template_manifests(repo.as_deref(), cli.global_root, &targets, &units),
        Command::Template {
            repo,
            command: TemplateCommand::GeneratedValues { targets, units },
        } => commands::run_template_generated_values(
            repo.as_deref(),
            cli.global_root,
            &targets,
            &units,
        ),
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
