use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use obelisk_k8s::ObeliskK8sError;
use obelisk_k8s::config::apply::Takeover;
use obelisk_k8s::config::diff::DiffOptions;
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
        /// Resolve `${{obelisk:aws-ssm:…}}` refs, as `apply` does (needs `AWS_SSM_ACCESS_KEY`,
        /// `AWS_SSM_SECRET_KEY` and `AWS_SSM_REGION`, or `AWS_SSM_PROFILE`); resolved values print
        /// masked (unless `--reveal`)
        #[arg(long, global = true)]
        resolve: bool,
        /// With `--resolve`: print resolved values in plaintext, not masked (errors too). Mind where
        /// the output goes (CI logs, scrollback)
        // Not `requires = "resolve"`: clap checks that per level, so it would refuse a `--resolve`
        // given on the other side of the subcommand (`template --reveal values --resolve`); `parse`
        // checks it.
        #[arg(long, global = true)]
        reveal: bool,

        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Install what `template manifests` renders: one Helm release per unit on each target, on its
    /// cluster's kubectl context
    Apply {
        /// The repo, by its name in the global `repos.yaml` [default: the one in the current directory]
        #[arg(long, help_heading = SPECIAL, value_name = "NAME")]
        repo: Option<String>,
        #[command(flatten)]
        args: TemplateArgs,
        /// Apply to remote clusters too (without it, a remote target is an error)
        #[arg(long)]
        yes: bool,
        /// Adopt resources already in the cluster that aren't the release's (e.g. applied by
        /// obelisk-local, but another release's too); a failed install is then left failed, not
        /// rolled back (that would delete what it adopted), nor by later runs while it stays failed
        #[arg(long)]
        take_ownership: bool,
        /// Take fields another field manager owns, forcing server-side apply on (which the release
        /// keeps); a one-off: it fights what the cluster manages, like an HPA's replicas
        #[arg(long)]
        force_conflicts: bool,
    },
    /// Show what `apply` would change, per unit on each target, against what's live on its
    /// cluster (with the helm-diff plugin); changes nothing
    Diff {
        /// The repo, by its name in the global `repos.yaml` [default: the one in the current directory]
        #[arg(long, help_heading = SPECIAL, value_name = "NAME")]
        repo: Option<String>,
        #[command(flatten)]
        args: TemplateArgs,
        /// Diff as `apply --take-ownership` would install: resources already in the cluster that
        /// aren't the release's show as adopted
        #[arg(long)]
        take_ownership: bool,
        /// Print resolved SSM values, and Secrets' data, in plaintext, not masked (errors too). Mind
        /// where the output goes (CI logs, scrollback)
        #[arg(long)]
        reveal: bool,
        /// Exit with 2 if anything would change (0 if nothing would; errors exit with 1)
        #[arg(long)]
        exit_code: bool,
    },
    /// Bring managed clusters up, down, and away (local k3d only, so far)
    Cluster {
        #[command(subcommand)]
        command: ManagedClusterCommand,
    },
    /// Print a shell completion script, e.g. `source <(obelisk-k8s completions bash)`
    Completions {
        /// The shell to complete for
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
enum ManagedClusterCommand {
    /// Show the clusters in the global config: kind, state (managed local k3d ones: running,
    /// stopped, missing, not ours), kubectl context and Docker network
    Status {
        /// Just this cluster [default: every cluster in the global config]
        name: Option<String>,
    },
    /// Create the cluster (or start it, if it's stopped), and add its kubectl context
    Up {
        /// The cluster, by its name in the global config
        name: String,
    },
    /// Stop the cluster, keeping its state for the next `up`
    Down {
        /// The cluster, by its name in the global config
        name: String,
    },
    /// Delete the cluster, everything in it, and its kubectl context
    Destroy {
        /// The cluster, by its name in the global config
        name: String,
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
    /// obelisk-k8s creates and destroys it (`cluster up` / `down` / `destroy`; local k3d only)
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
    /// The build plan each unit renders on each target: its build file (a Go template, rendered by
    /// `helm` with the `obelisk:` block as `.Values`), as the phases it lists
    Build(TemplateArgs),
    /// The values each unit ends up with on each target: every phase's values sources deep-merged in
    /// order over the `obelisk:` block, with the agreed release (SSM refs as written, unless
    /// `--resolve`)
    Values(TemplateArgs),
    /// The k8s resources each unit renders on each target: every phase's manifests sources, each
    /// rendered with the values of the phases before it, joined in order
    Manifests(TemplateArgs),
    /// The image specs each unit declares on each target: every phase's images sources, each
    /// rendered with the values of the phases before it (nothing builds them yet)
    Images(TemplateArgs),
}

impl TemplateCommand {
    fn args(&self) -> &TemplateArgs {
        match self {
            Self::Build(args) | Self::Values(args) | Self::Manifests(args) | Self::Images(args) => {
                args
            }
        }
    }
}

#[derive(Debug, Args)]
struct TemplateArgs {
    /// Only these targets (repeatable; default: all)
    #[arg(short, long = "target", value_name = "TARGET")]
    targets: Vec<String>,
    /// Only these units (repeatable; default: all)
    #[arg(short, long = "unit", value_name = "UNIT")]
    units: Vec<String>,
    #[command(flatten)]
    ephemeral: EphemeralArgs,
}

/// An ephemeral target's copy: needed exactly when a selected target is ephemeral.
#[derive(Debug, Args)]
struct EphemeralArgs {
    /// Which copy of an ephemeral target to render (`obelisk.ephemeral`; a DNS label)
    #[arg(
        id = "ephemeral_name",
        long = "ephemeral-name",
        value_name = "NAME",
        requires = "ephemeral_values_from"
    )]
    name: Option<String>,
    /// The copy's values, as YAML: merged over the values of every phase
    #[arg(
        id = "ephemeral_values",
        long = "ephemeral-values",
        value_name = "YAML",
        group = "ephemeral_values_from",
        requires = "ephemeral_name"
    )]
    values: Option<String>,
    /// The copy's values, from a YAML file (relative to the current directory): merged over the
    /// values of every phase
    #[arg(
        id = "ephemeral_values_path",
        long = "ephemeral-values-path",
        value_name = "PATH",
        group = "ephemeral_values_from",
        requires = "ephemeral_name"
    )]
    values_path: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Load and check the repo's config, then print its targets and units
    Show,
    /// Print the build file each unit renders on each target
    Paths {
        /// Only these targets (repeatable; default: all)
        #[arg(short, long = "target", value_name = "TARGET")]
        targets: Vec<String>,
        /// Only these units (repeatable; default: all)
        #[arg(short, long = "unit", value_name = "UNIT")]
        units: Vec<String>,
        /// Show each as a table: as written, where it's set, as evaluated, and whether it's there
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

/// The CLI parsed from `args` (the binary's name first), with the checks clap can't do itself.
fn parse<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut cmd = command();
    let cli = Cli::from_arg_matches(&cmd.try_get_matches_from_mut(args)?)?;
    if let Command::Template {
        resolve: false,
        reveal: true,
        ..
    } = cli.command
    {
        return Err(cmd.error(
            ErrorKind::MissingRequiredArgument,
            "`--reveal` needs `--resolve`",
        ));
    }
    Ok(cli)
}

fn main_impl() -> Result<ExitCode, ObeliskK8sError> {
    let cli = parse(std::env::args_os()).unwrap_or_else(|e| e.exit());
    let done = match cli.command {
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
            resolve,
            reveal,
            command,
        } => commands::run_template(repo.as_deref(), cli.global_root, resolve, reveal, &command),
        Command::Apply {
            repo,
            args,
            yes,
            take_ownership,
            force_conflicts,
        } => commands::run_apply(
            repo.as_deref(),
            cli.global_root,
            &args,
            yes,
            Takeover {
                take_ownership,
                force_conflicts,
            },
        ),
        // The one command whose exit code says more than whether it failed.
        Command::Diff {
            repo,
            args,
            take_ownership,
            reveal,
            exit_code,
        } => {
            return commands::run_diff(
                repo.as_deref(),
                cli.global_root,
                &args,
                DiffOptions {
                    take_ownership,
                    show_secrets: reveal,
                },
                exit_code,
            );
        }
        Command::Cluster { command } => commands::run_cluster(cli.global_root, &command),
        Command::Completions { shell } => {
            run_completions(shell);
            Ok(())
        }
    };
    done.map(|()| ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match main_impl() {
        Ok(code) => code,
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
    fn an_ephemeral_name_comes_with_one_kind_of_values() {
        let parse = |args: &[&str]| {
            let args = [BIN, "template", "values"].iter().chain(args);
            command().try_get_matches_from(args).map(|_| ())
        };
        assert!(parse(&[]).is_ok());
        assert!(parse(&["--ephemeral-name", "a", "--ephemeral-values", "{}"]).is_ok());
        assert!(parse(&["--ephemeral-name", "a", "--ephemeral-values-path", "v.yaml"]).is_ok());
        for bad in [
            &["--ephemeral-name", "a"][..],
            &["--ephemeral-values", "{}"],
            &["--ephemeral-values-path", "v.yaml"],
            &[
                "--ephemeral-name",
                "a",
                "--ephemeral-values",
                "{}",
                "--ephemeral-values-path",
                "v.yaml",
            ],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn template_renders_the_build_plan_the_values_and_the_manifests() {
        let parse = |sub: &str| super::parse([BIN, "template", sub]).map(|_| ());
        for sub in ["build", "values", "manifests"] {
            assert!(parse(sub).is_ok(), "{sub}");
        }
        // Its values are `values` now: they're after every phase.
        assert!(parse("generated-values").is_err());
        assert!(super::parse([BIN, "template", "--resolve", "build"]).is_ok());
    }

    #[test]
    fn diff_takes_applys_selection_but_never_needs_yes() {
        let parse = |args: &[&str]| super::parse([BIN, "diff"].iter().chain(args)).map(|_| ());
        assert!(parse(&[]).is_ok());
        assert!(
            parse(&[
                "-t",
                "prod",
                "-u",
                "app",
                "--take-ownership",
                "--reveal",
                "--exit-code",
                "--repo",
                "r",
            ])
            .is_ok()
        );
        assert!(parse(&["--ephemeral-name", "a", "--ephemeral-values", "{}"]).is_ok());
        // Read-only; and helm-diff has no `--force-conflicts`.
        assert!(parse(&["--yes"]).is_err());
        assert!(parse(&["--force-conflicts"]).is_err());
    }

    #[test]
    fn reveal_needs_resolve() {
        let parse = |args: &[&str]| super::parse([BIN, "template"].iter().chain(args)).map(|_| ());
        assert!(parse(&["manifests", "--resolve", "--reveal"]).is_ok());
        assert!(parse(&["--resolve", "--reveal", "values"]).is_ok());
        assert!(parse(&["--reveal", "values", "--resolve"]).is_ok());
        assert!(parse(&["--resolve", "values", "--reveal"]).is_ok());
        assert!(parse(&["manifests", "--reveal"]).is_err());
        assert!(parse(&["--reveal", "manifests"]).is_err());
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
