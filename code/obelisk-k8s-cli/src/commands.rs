//! What each command does: everything after the arguments are parsed. `main.rs` defines the CLI and
//! dispatches here.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use obelisk_k8s::config::apply::Takeover;
use obelisk_k8s::config::diff::DiffOptions;
use obelisk_k8s::config::ephemeral::{Ephemeral, EphemeralValues};
use obelisk_k8s::config::global::edit::{self as global_edit, Change};
use obelisk_k8s::config::global::{GLOBAL_ROOT_ENV, GlobalConfig, default_global_root, tilde};
use obelisk_k8s::config::pull_secret::PullSecretOptions;
use obelisk_k8s::config::repo::RepoConfig;
use obelisk_k8s::export::ExportFormat;
use obelisk_k8s::ssm::Ssm;
use obelisk_k8s::{ObeliskK8sError, cluster};

use crate::{
    ClusterCommand, DockerCommand, DockerPullCommand, DockerPushCommand, EphemeralArgs,
    GlobalCommand, GlobalRepoCommand, ImagesCommand, ManagedClusterCommand, TemplateArgs,
    TemplateCommand,
};

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

/// The repo to work on: the one named `repo` in the global config, else the current directory. With a
/// name, also where it is, as the user should see it.
fn repo_root(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
) -> Result<(PathBuf, Option<PathBuf>), ObeliskK8sError> {
    let Some(name) = repo else {
        return Ok((cwd()?, None));
    };
    let (root, home) = global_root(global_root_arg)?;
    let config = GlobalConfig::load(&root, home.as_deref())?;
    find_repo(name, &config, home.as_deref())
}

/// Repo `name` in `config`, and where it is as the user should see it.
fn find_repo(
    name: &str,
    config: &GlobalConfig,
    home: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>), ObeliskK8sError> {
    let found = config.repos.iter().find(|r| r.name == name).ok_or_else(|| {
        let known: Vec<_> = config.repos.iter().map(|r| r.name.as_str()).collect();
        let known = if known.is_empty() { "none".to_owned() } else { known.join(", ") };
        ObeliskK8sError::Usage(format!(
            "no repo `{name}` in {} (repos: {known}; `config global show` lists any that didn't load)",
            config.root.join("repos.yaml").display()
        ))
    })?;
    Ok((found.path.clone(), Some(tilde(&found.path, home))))
}

pub fn run_config_repo_show(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
) -> Result<(), ObeliskK8sError> {
    let (repo_root, _) = repo_root(repo, global_root_arg)?;
    print!("{}", load_repo_config(&repo_root)?);
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
        warn_one(warning);
    }
}

/// A warning, as it comes (e.g. a build plan's, before anything renders from it).
fn warn_one(warning: &str) {
    eprintln!("WARN - {warning}");
}

/// `export`: the values on stdin, a subtree exported to the file they name (or a temp file, its path
/// printed as values).
pub fn run_export(
    in_path: &str,
    out_path: &str,
    format: ExportFormat,
) -> Result<(), ObeliskK8sError> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|source| {
        ObeliskK8sError::Io {
            path: "stdin".into(),
            source,
        }
    })?;
    let out =
        obelisk_k8s::export::export(&input, in_path, out_path, format, &std::env::temp_dir())?;
    print!("{out}");
    Ok(())
}

/// `config global …`.
pub fn run_config_global(
    root: Option<PathBuf>,
    command: GlobalCommand,
) -> Result<(), ObeliskK8sError> {
    match command {
        GlobalCommand::Show => run_config_global_show(root),
        GlobalCommand::Init => run_config_global_init(root),
        GlobalCommand::Repo { command } => run_config_global_repo(root, command),
        GlobalCommand::Cluster { command } => run_config_global_cluster(root, command),
        GlobalCommand::Docker { command } => run_config_global_docker(root, command),
    }
}

/// `config global docker …`: a cluster's `docker.yaml`.
fn run_config_global_docker(
    root: Option<PathBuf>,
    command: DockerCommand,
) -> Result<(), ObeliskK8sError> {
    use obelisk_k8s::config::global::edit::docker::{
        self as docker_edit, AuthChange, NewCredentials, PushChange,
    };
    let (root, home) = global_root(root)?;
    let home = home.as_deref();
    let change = match command {
        DockerCommand::Show { cluster } => {
            let shown = docker_edit::show(&root, home, &cluster)?;
            warn(&shown.warnings);
            print!("{shown}");
            return Ok(());
        }
        DockerCommand::Remove { cluster } => docker_edit::remove(&root, home, &cluster)?,
        DockerCommand::Push {
            command:
                DockerPushCommand::Set {
                    cluster,
                    prefix,
                    username,
                    password,
                    password_stdin,
                },
        } => {
            let password = password_from(password, password_stdin)?;
            let credentials = match (username, password) {
                (Some(username), Some(password)) => Some(NewCredentials { username, password }),
                (None, None) => None,
                _ => {
                    return Err(ObeliskK8sError::Usage(
                        "give both `--username` and a password (`--password` or `--password-stdin`), or neither".to_owned(),
                    ));
                }
            };
            docker_edit::push_set(
                &root,
                home,
                &cluster,
                PushChange {
                    prefix,
                    credentials,
                },
            )?
        }
        DockerCommand::Push {
            command: DockerPushCommand::UnsetCredentials { cluster },
        } => docker_edit::push_unset_credentials(&root, home, &cluster)?,
        DockerCommand::Pull {
            command:
                DockerPullCommand::Add {
                    cluster,
                    host,
                    username,
                    password,
                    password_stdin,
                },
        } => {
            let password = password_from(password, password_stdin)?.unwrap_or_default();
            let credentials = NewCredentials { username, password };
            docker_edit::pull_add(&root, home, &cluster, &host, credentials)?
        }
        DockerCommand::Pull {
            command:
                DockerPullCommand::Set {
                    cluster,
                    host,
                    username,
                    password,
                    password_stdin,
                },
        } => {
            let password = password_from(password, password_stdin)?;
            let change = AuthChange { username, password };
            docker_edit::pull_set(&root, home, &cluster, &host, change)?
        }
        DockerCommand::Pull {
            command: DockerPullCommand::Remove { cluster, host },
        } => docker_edit::pull_remove(&root, home, &cluster, &host)?,
    };
    print_change(&change);
    Ok(())
}

/// `--password`, or (`--password-stdin`) stdin's first line.
fn password_from(
    password: Option<String>,
    from_stdin: bool,
) -> Result<Option<String>, ObeliskK8sError> {
    if !from_stdin {
        return Ok(password);
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(ObeliskK8sError::Usage(
            "`--password-stdin` reads the password from stdin: pipe it in".to_owned(),
        ));
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|source| ObeliskK8sError::Io {
            path: "stdin".into(),
            source,
        })?;
    let password = line.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        return Err(ObeliskK8sError::Usage(
            "`--password-stdin`: no password on stdin".to_owned(),
        ));
    }
    Ok(Some(password.to_owned()))
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

pub fn run_config_repo_paths(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    targets: &[String],
    units: &[String],
    explain: bool,
) -> Result<(), ObeliskK8sError> {
    let (repo_root, shown_root) = repo_root(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(targets, units)?;
    let mut report = config.paths_report(&repo_root, &selection, explain)?;
    report.shown_root = shown_root;
    // anstream drops the colors when stdout isn't a terminal, or with NO_COLOR set.
    anstream::print!("{report}");
    Ok(())
}

pub fn run_cluster(
    root: Option<PathBuf>,
    command: &ManagedClusterCommand,
) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    let config = GlobalConfig::load(&root, home.as_deref())?;
    warn(&config.warnings);
    let report = |report: cluster::Report| {
        warn(&report.warnings);
        println!("{report}");
    };
    match command {
        ManagedClusterCommand::Status { name } => {
            let status = cluster::status(&config, name.as_deref())?;
            warn(&status.warnings);
            print!("{status}");
        }
        ManagedClusterCommand::Up { name } => {
            report(cluster::up(&config, name, |step| eprintln!("{step}"))?);
        }
        ManagedClusterCommand::Down { name } => report(cluster::down(&config, name)?),
        ManagedClusterCommand::Destroy { name } => report(cluster::destroy(&config, name)?),
    }
    Ok(())
}

/// The global config (its warnings printed), and the repo to work on: the one named `repo` in it,
/// else the current directory. The clusters a repo's targets deploy to come from the global config,
/// so commands that render or apply load it either way, once.
fn global_and_repo(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
) -> Result<(GlobalConfig, PathBuf, Option<PathBuf>), ObeliskK8sError> {
    let (global_root, home) = global_root(global_root_arg)?;
    let global = GlobalConfig::load(&global_root, home.as_deref())?;
    warn(&global.warnings);
    let (repo_root, shown_root) = match repo {
        Some(name) => find_repo(name, &global, home.as_deref())?,
        None => (cwd()?, None),
    };
    Ok((global, repo_root, shown_root))
}

pub fn run_apply(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    args: &TemplateArgs,
    yes: bool,
    takeover: Takeover,
) -> Result<(), ObeliskK8sError> {
    let ssm = Ssm::from_env();
    run_apply_with(repo, global_root_arg, args, yes, takeover, &ssm)
        .map_err(|e| ssm.redact_error(e))
}

fn run_apply_with(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    args: &TemplateArgs,
    yes: bool,
    takeover: Takeover,
    ssm: &Ssm,
) -> Result<(), ObeliskK8sError> {
    let (global, repo_root, shown_root) = global_and_repo(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(&args.targets, &args.units)?;
    let ephemeral = ephemeral(&args.ephemeral)?;
    let report = config.apply(
        &repo_root,
        &selection,
        shown_root.as_deref(),
        ephemeral.as_ref(),
        &global,
        yes,
        takeover,
        ssm,
        warn_one,
        |step| eprintln!("{step}"),
    )?;
    print!("{}", ssm.redact(&report.to_string()));
    Ok(())
}

/// `images build`: builds the selected units' images (only `images`, if any) and, unless
/// `--no-push`, delivers them to their targets' clusters; each step on stderr, the result on stdout.
pub fn run_images(
    global_root_arg: Option<PathBuf>,
    command: &ImagesCommand,
) -> Result<(), ObeliskK8sError> {
    match command {
        ImagesCommand::Build {
            repo,
            args,
            images,
            no_push,
        } => run_images_build(global_root_arg, repo.as_deref(), args, images, *no_push),
        ImagesCommand::PullSecret {
            cluster,
            namespace,
            yes,
            force_conflicts,
            dry_run,
        } => {
            let options = PullSecretOptions {
                remote_ok: *yes,
                force_conflicts: *force_conflicts,
                dry_run: *dry_run,
            };
            run_pull_secret(global_root_arg, cluster, namespace, options)
        }
    }
}

/// `images build`: builds the selected units' images (only `images`, if any) and, unless
/// `no_push`, delivers them to their targets' clusters; each step on stderr, the result on stdout.
fn run_images_build(
    global_root_arg: Option<PathBuf>,
    repo: Option<&str>,
    args: &TemplateArgs,
    images: &[String],
    no_push: bool,
) -> Result<(), ObeliskK8sError> {
    let (global, repo_root, shown_root) = global_and_repo(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(&args.targets, &args.units)?;
    let ephemeral = ephemeral(&args.ephemeral)?;
    let (shown_root, ephemeral) = (shown_root.as_deref(), ephemeral.as_ref());
    let step = |step: &str| eprintln!("{step}");
    if no_push {
        let built = config.images_build(
            &repo_root, &selection, shown_root, ephemeral, &global, images, warn_one, step,
        )?;
        print!("{built}");
        return Ok(());
    }
    // Push credentials' refs resolve as they're needed; their values never print.
    let ssm = Ssm::from_env();
    let pushed = config
        .images_push(
            &repo_root, &selection, shown_root, ephemeral, &global, images, &ssm, warn_one, step,
        )
        .map_err(|e| ssm.redact_error(e))?;
    print!("{}", ssm.redact(&pushed.to_string()));
    Ok(())
}

/// `images pull-secret`: applies (or, `--dry-run`, prints) the pull secret in `namespace` on
/// `cluster`.
fn run_pull_secret(
    root: Option<PathBuf>,
    cluster: &str,
    namespace: &str,
    options: PullSecretOptions,
) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    let global = GlobalConfig::load(&root, home.as_deref())?;
    warn(&global.warnings);
    let ssm = Ssm::from_env();
    let applied = global
        .pull_secret(cluster, namespace, options, &ssm, |step| {
            eprintln!("{}", ssm.redact(step));
        })
        .map_err(|e| ssm.redact_error(e))?;
    print!("{}", ssm.redact(&applied.to_string()));
    Ok(())
}

/// `options.show_secrets` is `--reveal`: SSM values print in plaintext too. With `exit_code`
/// (`--exit-code`), exits with 1 if anything would change.
pub fn run_diff(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    args: &TemplateArgs,
    options: DiffOptions,
    exit_code: bool,
) -> Result<ExitCode, ObeliskK8sError> {
    let ssm = Ssm::from_env();
    let reveal = options.show_secrets;
    if reveal {
        eprintln!(
            "WARN - --reveal: resolved SSM values and Secrets' data are printed in plaintext"
        );
    }
    let (text, changed) = run_diff_with(repo, global_root_arg, args, options, &ssm)
        .map_err(|e| if reveal { e } else { ssm.redact_error(e) })?;
    let text = if reveal { text } else { ssm.redact(&text) };
    // anstream drops the colors when stdout isn't a terminal, or with NO_COLOR set.
    anstream::print!("{text}");
    Ok(if exit_code && changed {
        // Not 1: an error exits with that, and CI must tell drift from failure.
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

/// The diff report, as text, and whether anything would change.
fn run_diff_with(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    args: &TemplateArgs,
    options: DiffOptions,
    ssm: &Ssm,
) -> Result<(String, bool), ObeliskK8sError> {
    let (global, repo_root, shown_root) = global_and_repo(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(&args.targets, &args.units)?;
    let ephemeral = ephemeral(&args.ephemeral)?;
    let report = config.diff(
        &repo_root,
        &selection,
        shown_root.as_deref(),
        ephemeral.as_ref(),
        &global,
        options,
        ssm,
        warn_one,
        |step| eprintln!("{step}"),
    )?;
    Ok((report.to_string(), report.changed()))
}

pub fn run_template(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    resolve: bool,
    reveal: bool,
    command: &TemplateCommand,
) -> Result<(), ObeliskK8sError> {
    let ssm = resolve.then(Ssm::from_env);
    if reveal {
        eprintln!("WARN - --reveal: resolved SSM values are printed in plaintext");
    }
    let report = template_report(repo, global_root_arg, ssm.as_ref(), command);
    let report = match &ssm {
        Some(ssm) if !reveal => report
            .map(|r| ssm.redact(&r))
            .map_err(|e| ssm.redact_error(e))?,
        _ => report?,
    };
    // anstream drops the colors when stdout isn't a terminal, or with NO_COLOR set.
    anstream::print!("{report}");
    Ok(())
}

fn template_report(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    ssm: Option<&Ssm>,
    command: &TemplateCommand,
) -> Result<String, ObeliskK8sError> {
    let args = command.args();
    let (global, repo_root, shown_root) = global_and_repo(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(&args.targets, &args.units)?;
    let ephemeral = ephemeral(&args.ephemeral)?;
    let (shown_root, ephemeral) = (shown_root.as_deref(), ephemeral.as_ref());
    // Warnings are printed as each build plan is built: before the report, and even if a later
    // step fails.
    Ok(match command {
        // Nothing in a build plan is resolved: `--resolve` has nothing to do there.
        TemplateCommand::Build(_) => config
            .build_report(
                &repo_root, &selection, shown_root, ephemeral, &global, warn_one,
            )?
            .to_string(),
        TemplateCommand::Values(_) => config
            .values_report(
                &repo_root, &selection, shown_root, ephemeral, &global, ssm, warn_one,
            )?
            .to_string(),
        TemplateCommand::Manifests(_) => config
            .manifests_report(
                &repo_root, &selection, shown_root, ephemeral, &global, ssm, warn_one,
            )?
            .to_string(),
        // Nothing in an image spec is resolved, as in a build plan.
        TemplateCommand::Images(_) => config
            .images_report(
                &repo_root, &selection, shown_root, ephemeral, &global, warn_one,
            )?
            .to_string(),
    })
}

/// The ephemeral copy the flags name, if any.
fn ephemeral(args: &EphemeralArgs) -> Result<Option<Ephemeral>, ObeliskK8sError> {
    let Some(name) = &args.name else {
        return Ok(None);
    };
    let values = match (&args.values, &args.values_path) {
        (Some(text), _) => EphemeralValues::Inline(text.clone()),
        (None, Some(path)) => EphemeralValues::File(path.clone()),
        // clap already requires one with the name.
        (None, None) => {
            return Err(ObeliskK8sError::Usage(
                "`--ephemeral-name` needs `--ephemeral-values` or `--ephemeral-values-path`"
                    .to_owned(),
            ));
        }
    };
    Ephemeral::new(name.clone(), values).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
