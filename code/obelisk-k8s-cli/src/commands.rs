//! What each command does: everything after the arguments are parsed. `main.rs` defines the CLI and
//! dispatches here.

use std::path::{Path, PathBuf};

use obelisk_k8s::config::ephemeral::{Ephemeral, EphemeralValues};
use obelisk_k8s::config::global::edit::{self as global_edit, Change};
use obelisk_k8s::config::global::{GLOBAL_ROOT_ENV, GlobalConfig, default_global_root, tilde};
use obelisk_k8s::config::repo::RepoConfig;
use obelisk_k8s::{ObeliskK8sError, cluster};

use crate::{
    ClusterCommand, EphemeralArgs, GlobalRepoCommand, ManagedClusterCommand, TemplateArgs,
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
        eprintln!("WARN - {warning}");
    }
}

pub fn run_config_global_show(root: Option<PathBuf>) -> Result<(), ObeliskK8sError> {
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

pub fn run_config_global_init(root: Option<PathBuf>) -> Result<(), ObeliskK8sError> {
    let (root, home) = global_root(root)?;
    print_change(&global_edit::init(&root, home.as_deref())?);
    Ok(())
}

pub fn run_config_global_repo(
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

pub fn run_config_global_cluster(
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

pub fn run_apply(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    args: &TemplateArgs,
    yes: bool,
) -> Result<(), ObeliskK8sError> {
    // The clusters come from the global config, and so does the repo with `--repo`: load it once.
    let (global_root, home) = global_root(global_root_arg)?;
    let global = GlobalConfig::load(&global_root, home.as_deref())?;
    warn(&global.warnings);
    let (repo_root, shown_root) = match repo {
        Some(name) => find_repo(name, &global, home.as_deref())?,
        None => (cwd()?, None),
    };
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
        |step| eprintln!("{step}"),
    )?;
    print!("{report}");
    Ok(())
}

pub fn run_template(
    repo: Option<&str>,
    global_root_arg: Option<PathBuf>,
    command: &TemplateCommand,
) -> Result<(), ObeliskK8sError> {
    let args = command.args();
    let (repo_root, shown_root) = repo_root(repo, global_root_arg)?;
    let config = load_repo_config(&repo_root)?;
    let selection = config.select(&args.targets, &args.units)?;
    let ephemeral = ephemeral(&args.ephemeral)?;
    let (shown_root, ephemeral) = (shown_root.as_deref(), ephemeral.as_ref());
    let report = match command {
        TemplateCommand::Values(_) => config
            .values_report(&repo_root, &selection, shown_root, ephemeral)?
            .to_string(),
        TemplateCommand::GeneratedValues(_) => config
            .generated_values_report(&repo_root, &selection, shown_root, ephemeral)?
            .to_string(),
        TemplateCommand::Manifests(_) => config
            .manifests_report(&repo_root, &selection, shown_root, ephemeral)?
            .to_string(),
    };
    // anstream drops the colors when stdout isn't a terminal, or with NO_COLOR set.
    anstream::print!("{report}");
    Ok(())
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
