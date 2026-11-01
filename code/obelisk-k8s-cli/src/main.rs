use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use obelisk_k8s::ObeliskK8sError;
use obelisk_k8s::config::global::edit::{self as global_edit, Change, NewCluster};
use obelisk_k8s::config::global::{
    GLOBAL_ROOT_ENV, GlobalConfig, K8s, Location, Obeliskd, default_global_root,
};
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

/// The global root, and the home directory (for `~`).
fn global_root(
    global_root: Option<PathBuf>,
) -> Result<(PathBuf, Option<PathBuf>), ObeliskK8sError> {
    let home = home();
    let root = global_root
        .map(|root| expand_tilde(root, home.as_deref()))
        .or_else(|| default_global_root(home.as_deref()))
        .ok_or_else(|| {
            ObeliskK8sError::Usage(format!(
                "no home directory for the default global root: set `--global-root` or `{GLOBAL_ROOT_ENV}`"
            ))
        })?;
    Ok((root, home))
}

fn warn(warnings: &[String]) {
    for warning in warnings {
        eprintln!("WARN - {warning}");
    }
}

fn run_config_global_show(root: Option<PathBuf>) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    let config = GlobalConfig::load(&root, home.as_deref())?;
    warn(&config.warnings);
    print!("{config}");
    Ok(())
}

fn print_change(change: &Change) {
    warn(&change.warnings);
    println!("{}", change.message);
}

fn run_config_global_init(root: Option<PathBuf>) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    print_change(&global_edit::init(&root, home.as_deref())?);
    Ok(())
}

fn run_config_global_repo(
    root: Option<PathBuf>,
    command: GlobalRepoCommand,
) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    let home = home.as_deref();
    print_change(&match command {
        GlobalRepoCommand::Add { path } => global_edit::add_repo(&root, home, &path)?,
        GlobalRepoCommand::Remove { name_or_path } => {
            global_edit::remove_repo(&root, home, &name_or_path)?
        }
    });
    Ok(())
}

fn run_config_global_cluster(
    root: Option<PathBuf>,
    command: ClusterCommand,
) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    let home = home.as_deref();
    print_change(&match command {
        ClusterCommand::Add(add) => global_edit::add_cluster(&root, home, &add.into())?,
        ClusterCommand::Remove { name } => global_edit::remove_cluster(&root, home, &name)?,
    });
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
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Init,
                },
        } => run_config_global_init(cli.global_root),
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Cluster { command },
                },
        } => run_config_global_cluster(cli.global_root, command),
        Command::Config {
            command:
                ConfigCommand::Global {
                    command: GlobalCommand::Repo { command },
                },
        } => run_config_global_repo(cli.global_root, command),
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
