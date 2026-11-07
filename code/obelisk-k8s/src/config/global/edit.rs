//! Changing the machine-wide config: `config global init`, `cluster add` / `remove`, `repo add` /
//! `remove`.
//!
//! The cluster commands only create or delete files. The repo commands edit `repos.yaml` in place,
//! keeping its comments and formatting (creating it if it's missing). Every change is checked by
//! loading the global config again, and undone if that fails. A cluster's `docker.yaml`: see
//! `docker`.

pub mod docker;

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    GlobalConfig, K8s, Location, Obeliskd, Repo, absolute_root, is_repo, list_repos,
    read_repo_entries, resolve_entry, tilde,
};
use crate::ObeliskK8sError;
use crate::config::repo::REPO_CONFIG_DIR;
use crate::config::{NAME_RULE, is_valid_name};
use crate::yaml::{self, Spanned};

/// What a change did, in words, and the global config's warnings afterwards.
#[derive(Debug)]
pub struct Change {
    pub message: String,
    pub warnings: Vec<String>,
}

/// Sets up the global root: the directory, an empty `repos.yaml` and `clusters/`, each only if it's
/// missing. Then loads it, so an existing root that's broken fails here.
///
/// # Errors
///
/// [`ObeliskK8sError::Io`] if something can't be created; whatever loading the global config fails
/// with.
pub fn init(root: &Path, home: Option<&Path>) -> Result<Change, ObeliskK8sError> {
    let shown = tilde(root, home);
    let io_error = |path: PathBuf| move |source| ObeliskK8sError::Io { path, source };
    let mut created = Vec::new();
    if !root.is_dir() {
        std::fs::create_dir_all(root).map_err(io_error(shown.clone()))?;
        created.push(shown.clone());
    }
    let repos = root.join("repos.yaml");
    if !repos.exists() {
        std::fs::write(&repos, "repos: []\n").map_err(io_error(shown.join("repos.yaml")))?;
        created.push(shown.join("repos.yaml"));
    }
    let clusters = root.join("clusters");
    if !clusters.is_dir() {
        std::fs::create_dir(&clusters).map_err(io_error(shown.join("clusters")))?;
        created.push(shown.join("clusters"));
    }
    let loaded = GlobalConfig::load(root, home)?;
    let message = if created.is_empty() {
        format!("already set up: {}", shown.display())
    } else {
        let created: Vec<_> = created.iter().map(|p| p.display().to_string()).collect();
        format!("created {}", created.join(", "))
    };
    Ok(Change {
        message,
        warnings: loaded.warnings,
    })
}

/// A cluster to add, as `cluster add` takes it.
#[derive(Debug, Clone)]
pub struct NewCluster {
    pub name: String,
    pub k8s: K8s,
    pub location: Location,
    pub managed: bool,
    pub obeliskd: Option<Obeliskd>,
    pub kubectl_cluster: Option<String>,
    pub kubectl_context: Option<String>,
}

/// Writes `clusters/<name>/cluster.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for a bad or taken name, or `obeliskd` on an unmanaged cluster; and
/// whatever loading the global config fails with (before or after: after, the directory is removed).
pub fn add_cluster(
    root: &Path,
    home: Option<&Path>,
    cluster: &NewCluster,
) -> Result<Change, ObeliskK8sError> {
    let config = GlobalConfig::load(root, home)?;
    let name = &cluster.name;
    if !is_valid_name(name) {
        return Err(usage(format!(
            "`{name}` isn't a valid cluster name ({NAME_RULE})"
        )));
    }
    let dir = root.join("clusters").join(name);
    let shown_dir = tilde(&dir, home);
    if let Some(taken) = config.clusters.iter().find(|c| &c.name == name) {
        return Err(usage(format!(
            "cluster `{name}` already exists ({})",
            taken.file.display()
        )));
    }
    if dir.exists() {
        return Err(usage(format!("{} already exists", shown_dir.display())));
    }
    if cluster.obeliskd.is_some() && !cluster.managed {
        return Err(usage(
            "`--obeliskd-*` is only for a managed cluster (`--managed`)".to_owned(),
        ));
    }

    let file = dir.join("cluster.yaml");
    let shown = tilde(&file, home);
    std::fs::create_dir_all(&dir).map_err(|source| ObeliskK8sError::Io {
        path: shown_dir.clone(),
        source,
    })?;
    let out = ClusterFileOut {
        cluster: ClusterOut::from(cluster),
    };
    let loaded =
        yaml::write_new_file(&file, &shown, &out).and_then(|()| GlobalConfig::load(root, home));
    let after = loaded.map_err(|error| {
        let undone = std::fs::remove_dir_all(&dir);
        undo_failed(error, undone, &shown_dir)
    })?;
    Ok(Change {
        message: format!("wrote {}", shown.display()),
        warnings: after.warnings,
    })
}

/// Deletes `name`'s `clusters/<dir>/`, which must hold nothing but its `cluster.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster, or a directory with other files in it; and
/// whatever loading the global config fails with.
pub fn remove_cluster(
    root: &Path,
    home: Option<&Path>,
    name: &str,
) -> Result<Change, ObeliskK8sError> {
    let config = GlobalConfig::load(root, home)?;
    let cluster = config.cluster(name)?;
    let shown = tilde(&cluster.dir, home);
    let io_error = |source| ObeliskK8sError::Io {
        path: shown.clone(),
        source,
    };
    let mut others: Vec<String> = std::fs::read_dir(&cluster.dir)
        .map_err(io_error)?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "cluster.yaml")
        .collect();
    if !others.is_empty() {
        others.sort();
        return Err(usage(format!(
            "{} holds more than its cluster.yaml ({}): remove those first{}",
            shown.display(),
            others.join(", "),
            if others.iter().any(|o| o == "docker.yaml") {
                format!(" (`config global docker remove --cluster {name}` for its docker.yaml)")
            } else {
                String::new()
            }
        )));
    }
    std::fs::remove_dir_all(&cluster.dir).map_err(io_error)?;
    Ok(Change {
        message: format!("removed {}", shown.display()),
        warnings: GlobalConfig::load(root, home)?.warnings,
    })
}

/// `repos.yaml`, and what's needed to change it.
struct ReposFile<'a> {
    root: &'a Path,
    home: Option<&'a Path>,
    /// `root`, absolute: relative entries resolve from it.
    abs_root: PathBuf,
    /// As the user should see it.
    shown: PathBuf,
    config: GlobalConfig,
    entries: Vec<Spanned<String>>,
}

impl<'a> ReposFile<'a> {
    fn load(root: &'a Path, home: Option<&'a Path>) -> Result<Self, ObeliskK8sError> {
        let config = GlobalConfig::load(root, home)?;
        let shown_root = tilde(root, home);
        Ok(Self {
            abs_root: absolute_root(root, &shown_root)?,
            entries: read_repo_entries(root, &shown_root)?,
            shown: shown_root.join("repos.yaml"),
            root,
            home,
            config,
        })
    }

    fn resolve(&self, entry: &str) -> Option<super::EntryTarget> {
        resolve_entry(entry, &self.abs_root, self.home).ok()
    }

    /// The first entry other than `entry` itself that resolves to the same place.
    fn same_place(&self, entry: &str) -> Option<&str> {
        let target = self.resolve(entry)?;
        self.entries
            .iter()
            .find(|e| e.value != entry && self.resolve(&e.value).as_ref() == Some(&target))
            .map(|e| e.value.as_str())
    }

    /// The repos an entry points at, by path.
    fn paths(&self, entry: &str) -> Vec<PathBuf> {
        match self.resolve(entry) {
            Some(t) if t.wildcard => list_repos(&t.dir).unwrap_or_default(),
            Some(t) if is_repo(&t.dir) => vec![t.dir],
            _ => Vec::new(),
        }
    }

    fn text(&self) -> Result<String, ObeliskK8sError> {
        let path = self.root.join("repos.yaml");
        if !path.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&path).map_err(|source| ObeliskK8sError::Io {
            path: self.shown.clone(),
            source,
        })
    }

    /// Writes `edited` (through a temp file and a rename) and loads the global config again; if that
    /// fails, puts `original` back.
    fn write(&self, original: &str, edited: &str) -> Result<GlobalConfig, ObeliskK8sError> {
        let path = self.root.join("repos.yaml");
        let existed = path.exists();
        let write = |text: &str| {
            let temp = self
                .root
                .join(format!(".repos.yaml.{}.tmp", std::process::id()));
            std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &path))
        };
        write(edited).map_err(|source| ObeliskK8sError::Io {
            path: self.shown.clone(),
            source,
        })?;
        GlobalConfig::load(self.root, self.home).map_err(|error| {
            let undone = if existed {
                write(original)
            } else {
                std::fs::remove_file(&path)
            };
            undo_failed(error, undone, &self.shown)
        })
    }
}

/// Appends `entry` to `repos.yaml` (created if it's missing), as written.
///
/// It must point at a repo (a directory with a `.obelisk-k8s/`), or end in `/*` and point at a
/// readable directory. An entry already listed, or one listing the same place spelled differently,
/// changes nothing. A repo a `/*` entry already covers is added anyway, with a note.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an entry that doesn't point at a repo (or a readable directory);
/// whatever loading the global config fails with, before or after (after: `repos.yaml` is put back).
pub fn add_repo(root: &Path, home: Option<&Path>, entry: &str) -> Result<Change, ObeliskK8sError> {
    let repos = ReposFile::load(root, home)?;
    let file = repos.shown.display();
    let unchanged = |message: String| {
        Ok(Change {
            message,
            warnings: repos.config.warnings.clone(),
        })
    };
    if repos.entries.iter().any(|e| e.value == entry) {
        return unchanged(format!("`{entry}` is already in {file}"));
    }
    let target = resolve_entry(entry, &repos.abs_root, home).map_err(usage)?;
    let shown_dir = tilde(&target.dir, home);
    if target.wildcard && list_repos(&target.dir).is_none() {
        return Err(usage(format!(
            "`{entry}`: can't read {}",
            shown_dir.display()
        )));
    }
    if !target.wildcard && !is_repo(&target.dir) {
        return Err(usage(format!(
            "`{entry}`: no `{REPO_CONFIG_DIR}/` in {}",
            shown_dir.display()
        )));
    }
    if let Some(same) = repos.same_place(entry) {
        return unchanged(format!("{file} already lists that, as `{same}`"));
    }
    let covered_by = (!target.wildcard)
        .then(|| {
            repos.entries.iter().find(|e| {
                repos
                    .resolve(&e.value)
                    .is_some_and(|o| o.wildcard && target.dir.parent() == Some(o.dir.as_path()))
            })
        })
        .flatten();

    let text = repos.text()?;
    let edited = yaml::push_to_list(&text, &repos.shown, "repos", entry)?;
    let after = repos.write(&text, &edited)?;
    let paths = repos.paths(entry);
    let loaded = names(after.repos.iter().filter(|r| paths.contains(&r.path)));
    let what = match (loaded.is_empty(), paths.is_empty()) {
        (false, _) => loaded,
        (true, true) => "(no repos there yet)".to_owned(),
        (true, false) => "(it won't load: see the warning)".to_owned(),
    };
    let note = covered_by
        .map(|e| format!(" (`{}` already covers it)", e.value))
        .unwrap_or_default();
    Ok(Change {
        message: format!("added `{entry}` to {file}: {what}{note}"),
        warnings: after.warnings,
    })
}

/// Removes an entry from `repos.yaml`: the one written exactly as `name_or_entry`, else the one listing
/// the same place spelled differently, else the one listing the repo named `name_or_entry` by path. A
/// repo only a `/*` entry brings in can't be removed on its own.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if there's no such entry or repo, or the repo only comes from a `/*`
/// entry; whatever loading the global config fails with, before or after (after: `repos.yaml` is put
/// back).
pub fn remove_repo(
    root: &Path,
    home: Option<&Path>,
    name_or_entry: &str,
) -> Result<Change, ObeliskK8sError> {
    let repos = ReposFile::load(root, home)?;
    let index = entry_to_remove(&repos, name_or_entry)?;
    let entry = &repos.entries[index];

    let text = repos.text()?;
    let edited = yaml::remove_from_list(&text, &repos.shown, "repos", index, entry.at.line)?;
    let after = repos.write(&text, &edited)?;
    let paths = repos.paths(&entry.value);
    let gone = |r: &&Repo| !after.repos.iter().any(|a| a.path == r.path);
    let removed = names(
        repos
            .config
            .repos
            .iter()
            .filter(|r| paths.contains(&r.path))
            .filter(gone),
    );
    let kept: Vec<_> = after
        .repos
        .iter()
        .filter(|r| paths.contains(&r.path))
        .map(|r| format!("`{}` is still listed by `{}`", r.name, r.from))
        .collect();
    let what = match (removed.is_empty(), kept.is_empty()) {
        (true, true) => "it listed no repos".to_owned(),
        (false, true) => removed,
        (true, false) => kept.join(", "),
        (false, false) => format!("{removed} ({})", kept.join(", ")),
    };
    Ok(Change {
        message: format!(
            "removed `{}` from {}: {what}",
            entry.value,
            repos.shown.display()
        ),
        warnings: after.warnings,
    })
}

/// The index of the entry `remove_repo` removes.
fn entry_to_remove(repos: &ReposFile<'_>, name_or_entry: &str) -> Result<usize, ObeliskK8sError> {
    let position = |value: &str| repos.entries.iter().position(|e| e.value == value);
    if let Some(index) = position(name_or_entry) {
        return Ok(index);
    }
    if let Some(index) = repos.same_place(name_or_entry).and_then(position) {
        return Ok(index);
    }
    let Some(repo) = repos.config.repos.iter().find(|r| r.name == name_or_entry) else {
        let listed: Vec<_> = repos
            .entries
            .iter()
            .map(|e| format!("`{}`", e.value))
            .collect();
        let listed = if listed.is_empty() {
            "none".to_owned()
        } else {
            listed.join(", ")
        };
        return Err(usage(format!(
            "no repo or entry `{name_or_entry}` in {} (entries: {listed})",
            repos.shown.display()
        )));
    };
    repos
        .entries
        .iter()
        .position(|e| {
            repos
                .resolve(&e.value)
                .is_some_and(|t| !t.wildcard && t.dir == repo.path)
        })
        .ok_or_else(|| {
            usage(format!(
                "repo `{}` comes from `{}`: remove that entry, or move the repo out of it",
                repo.name, repo.from
            ))
        })
}

/// "repo `a`" / "repos `a`, `b`" (empty for none).
fn names<'a>(repos: impl Iterator<Item = &'a Repo>) -> String {
    let names: Vec<_> = repos.map(|r| format!("`{}`", r.name)).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => format!("repo {one}"),
        many => format!("repos {}", many.join(", ")),
    }
}

/// `error`, plus a note if undoing the change failed too (the change is still there to clean up).
fn undo_failed(
    error: ObeliskK8sError,
    undone: std::io::Result<()>,
    what: &Path,
) -> ObeliskK8sError {
    match undone {
        Ok(()) => error,
        Err(undo) => usage(format!(
            "{error}\n(and undoing the change to {} failed: {undo})",
            what.display()
        )),
    }
}

fn usage(message: String) -> ObeliskK8sError {
    ObeliskK8sError::Usage(message)
}

// The file, as written. The name comes from the directory, so it isn't written.

#[derive(Serialize)]
struct ClusterFileOut<'a> {
    cluster: ClusterOut<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClusterOut<'a> {
    k8s: K8s,
    location: Location,
    managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    obeliskd: Option<&'a Obeliskd>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kubectl_cluster: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kubectl_context: Option<&'a str>,
}

impl<'a> From<&'a NewCluster> for ClusterOut<'a> {
    fn from(c: &'a NewCluster) -> Self {
        Self {
            k8s: c.k8s,
            location: c.location,
            managed: c.managed,
            obeliskd: c.obeliskd.as_ref(),
            kubectl_cluster: c.kubectl_cluster.as_deref(),
            kubectl_context: c.kubectl_context.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(name: &str) -> NewCluster {
        NewCluster {
            name: name.to_owned(),
            k8s: K8s::K3d,
            location: Location::Local,
            managed: true,
            obeliskd: Some(Obeliskd {
                supernet: "sn".to_owned(),
                subnet: "s1".to_owned(),
            }),
            kubectl_cluster: None,
            kubectl_context: Some("k3d-dev".to_owned()),
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".obelisk-k8s");
        init(&root, Some(home.path())).unwrap();
        (home, root)
    }

    #[test]
    fn init_creates_whats_missing() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".obelisk-k8s");
        let first = init(&root, Some(home.path())).unwrap();
        assert_eq!(
            first.message,
            "created ~/.obelisk-k8s, ~/.obelisk-k8s/repos.yaml, ~/.obelisk-k8s/clusters"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("repos.yaml")).unwrap(),
            "repos: []\n"
        );
        assert_eq!(
            init(&root, Some(home.path())).unwrap().message,
            "already set up: ~/.obelisk-k8s"
        );
        let config = GlobalConfig::load(&root, Some(home.path())).unwrap();
        assert_eq!((config.repos.len(), config.clusters.len()), (0, 0));
    }

    #[test]
    fn adds_a_cluster_that_loads_back() {
        let (home, root) = setup();
        let changed = add_cluster(&root, Some(home.path()), &cluster("dev")).unwrap();
        assert_eq!(
            changed.message,
            "wrote ~/.obelisk-k8s/clusters/dev/cluster.yaml"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("clusters/dev/cluster.yaml")).unwrap(),
            "cluster:\n  k8s: k3d\n  location: local\n  managed: true\n  obeliskd:\n    supernet: sn\n    subnet: s1\n  kubectlContext: k3d-dev\n"
        );
        let c = &GlobalConfig::load(&root, Some(home.path()))
            .unwrap()
            .clusters[0];
        assert_eq!(
            (c.name.as_str(), c.managed, c.kubectl_context.as_deref()),
            ("dev", true, Some("k3d-dev"))
        );
    }

    #[test]
    fn quotes_values_that_need_it() {
        let (home, root) = setup();
        let mut c = cluster("dev");
        c.kubectl_context = Some("yes".to_owned());
        add_cluster(&root, Some(home.path()), &c).unwrap();
        let loaded = GlobalConfig::load(&root, Some(home.path())).unwrap();
        assert_eq!(loaded.clusters[0].kubectl_context.as_deref(), Some("yes"));
    }

    #[test]
    fn warns_about_a_remote_cluster_with_no_context() {
        let (home, root) = setup();
        let mut c = cluster("prod");
        (c.location, c.managed, c.obeliskd, c.kubectl_context) =
            (Location::Remote, false, None, None);
        let changed = add_cluster(&root, Some(home.path()), &c).unwrap();
        assert_eq!(changed.warnings.len(), 1, "{:?}", changed.warnings);
    }

    #[test]
    fn refuses_bad_clusters() {
        let (home, root) = setup();
        let error = |c: &NewCluster| {
            add_cluster(&root, Some(home.path()), c)
                .unwrap_err()
                .to_string()
        };
        assert!(error(&cluster("Dev")).contains("`Dev` isn't a valid cluster name"));
        let mut unmanaged = cluster("dev");
        unmanaged.managed = false;
        assert_eq!(
            error(&unmanaged),
            "`--obeliskd-*` is only for a managed cluster (`--managed`)"
        );
        assert!(!root.join("clusters/dev").exists());

        add_cluster(&root, Some(home.path()), &cluster("dev")).unwrap();
        assert_eq!(
            error(&cluster("dev")),
            "cluster `dev` already exists (~/.obelisk-k8s/clusters/dev/cluster.yaml)"
        );
        // A cluster named `dev` in another directory takes the name too.
        std::fs::create_dir_all(root.join("clusters/other")).unwrap();
        std::fs::write(
            root.join("clusters/other/cluster.yaml"),
            "cluster:\n  name: named\n  k8s: k3d\n  location: local\n  managed: false\n",
        )
        .unwrap();
        assert!(error(&cluster("named")).contains("cluster `named` already exists"));
        // A stray directory with the name.
        std::fs::create_dir_all(root.join("clusters/stray")).unwrap();
        assert_eq!(
            error(&cluster("stray")),
            "~/.obelisk-k8s/clusters/stray already exists"
        );
    }

    #[test]
    fn needs_the_root() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".obelisk-k8s");
        let error = add_cluster(&root, Some(home.path()), &cluster("dev")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("run `obelisk-k8s config global init`"),
            "{error}"
        );
    }

    #[test]
    fn removes_a_cluster_only_if_its_directory_holds_just_its_file() {
        let (home, root) = setup();
        add_cluster(&root, Some(home.path()), &cluster("dev")).unwrap();
        std::fs::write(root.join("clusters/dev/notes.txt"), "").unwrap();
        let error = remove_cluster(&root, Some(home.path()), "dev")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "~/.obelisk-k8s/clusters/dev holds more than its cluster.yaml (notes.txt): remove those first"
        );
        std::fs::remove_file(root.join("clusters/dev/notes.txt")).unwrap();
        let changed = remove_cluster(&root, Some(home.path()), "dev").unwrap();
        assert_eq!(changed.message, "removed ~/.obelisk-k8s/clusters/dev");
        assert!(!root.join("clusters/dev").exists());
        assert_eq!(
            remove_cluster(&root, Some(home.path()), "dev")
                .unwrap_err()
                .to_string(),
            "no cluster `dev` (there are: none)"
        );
    }

    #[test]
    fn removes_a_cluster_by_name_not_directory() {
        let (home, root) = setup();
        std::fs::create_dir_all(root.join("clusters/other")).unwrap();
        std::fs::write(
            root.join("clusters/other/cluster.yaml"),
            "cluster:\n  name: named\n  k8s: k3d\n  location: local\n  managed: false\n",
        )
        .unwrap();
        remove_cluster(&root, Some(home.path()), "named").unwrap();
        assert!(!root.join("clusters/other").exists());
    }

    /// `~/<dir>` holding a repo named `name`.
    fn repo(home: &Path, dir: &str, name: &str) {
        let config = home.join(dir).join(".obelisk-k8s");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("repo.yaml"), format!("repo:\n  name: {name}\n")).unwrap();
    }

    fn repos_yaml(root: &Path) -> String {
        std::fs::read_to_string(root.join("repos.yaml")).unwrap()
    }

    #[test]
    fn adds_repos_keeping_the_file_as_written() {
        let (home, root) = setup();
        let h = Some(home.path());
        repo(home.path(), "dev/a", "a");
        repo(home.path(), "work/c", "c");
        repo(home.path(), "work/d", "d");
        std::fs::write(
            root.join("repos.yaml"),
            "# mine\nrepos:\n  - ../dev/a # main\n\n# footer\n",
        )
        .unwrap();

        let add = |entry: &str| add_repo(&root, h, entry).unwrap().message;
        assert_eq!(
            add("~/work/*"),
            "added `~/work/*` to ~/.obelisk-k8s/repos.yaml: repos `c`, `d`"
        );
        assert_eq!(
            add("../work/c"),
            "added `../work/c` to ~/.obelisk-k8s/repos.yaml: repo `c` (`~/work/*` already covers it)"
        );
        assert_eq!(
            add("../work/c"),
            "`../work/c` is already in ~/.obelisk-k8s/repos.yaml"
        );
        assert_eq!(
            add("~/dev/a"),
            "~/.obelisk-k8s/repos.yaml already lists that, as `../dev/a`"
        );
        assert_eq!(
            repos_yaml(&root),
            "# mine\nrepos:\n  - ../dev/a # main\n  - '~/work/*'\n  - ../work/c\n\n# footer\n"
        );
    }

    #[test]
    fn adds_to_a_missing_or_empty_list() {
        let (home, root) = setup();
        repo(home.path(), "a", "a");
        add_repo(&root, Some(home.path()), "~/a").unwrap();
        assert_eq!(repos_yaml(&root), "repos: [~/a]\n");

        std::fs::remove_file(root.join("repos.yaml")).unwrap();
        add_repo(&root, Some(home.path()), "~/a").unwrap();
        assert_eq!(repos_yaml(&root), "repos:\n  - ~/a\n");

        std::fs::write(root.join("repos.yaml"), "# nothing yet\n").unwrap();
        add_repo(&root, Some(home.path()), "~/a").unwrap();
        assert_eq!(repos_yaml(&root), "# nothing yet\nrepos:\n  - ~/a\n");
    }

    #[test]
    fn refuses_entries_that_dont_point_at_repos() {
        let (home, root) = setup();
        let error = |entry: &str| {
            add_repo(&root, Some(home.path()), entry)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(error("~/nope"), "`~/nope`: no `.obelisk-k8s/` in ~/nope");
        assert_eq!(error("~/nope/*"), "`~/nope/*`: can't read ~/nope");
        assert!(error("~/a/*/b").contains("`*` is only allowed as a whole last component"));
        assert_eq!(repos_yaml(&root), "repos: []\n");
    }

    #[test]
    fn puts_the_file_back_if_the_result_wont_load() {
        let (home, root) = setup();
        repo(home.path(), "a", "same");
        repo(home.path(), "b", "same");
        add_repo(&root, Some(home.path()), "~/a").unwrap();
        let before = repos_yaml(&root);
        let error = add_repo(&root, Some(home.path()), "~/b")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("repo name `same` is already used"),
            "{error}"
        );
        assert_eq!(repos_yaml(&root), before);
    }

    #[test]
    fn removes_repos_with_the_comment_above() {
        let (home, root) = setup();
        let h = Some(home.path());
        repo(home.path(), "a", "a");
        repo(home.path(), "b", "b");
        repo(home.path(), "w/c", "c");
        std::fs::write(
            root.join("repos.yaml"),
            "repos:\n  # about a\n  # (more)\n  - ~/a # main\n  - ~/b\n  - \"~/w/*\"\n  - ~/w/c\n# footer\n",
        )
        .unwrap();
        let remove = |what: &str| remove_repo(&root, h, what).unwrap().message;
        assert_eq!(
            remove("a"),
            "removed `~/a` from ~/.obelisk-k8s/repos.yaml: repo `a`"
        );
        assert_eq!(
            remove("~/w/c"),
            "removed `~/w/c` from ~/.obelisk-k8s/repos.yaml: `c` is still listed by `~/w/*`"
        );
        assert_eq!(
            repos_yaml(&root),
            "repos:\n  - ~/b\n  - \"~/w/*\"\n# footer\n"
        );

        let error = remove_repo(&root, h, "c").unwrap_err().to_string();
        assert_eq!(
            error,
            "repo `c` comes from `~/w/*`: remove that entry, or move the repo out of it"
        );
        let error = remove_repo(&root, h, "zzz").unwrap_err().to_string();
        assert_eq!(
            error,
            "no repo or entry `zzz` in ~/.obelisk-k8s/repos.yaml (entries: `~/b`, `~/w/*`)"
        );
        assert_eq!(
            remove("~/w/*"),
            "removed `~/w/*` from ~/.obelisk-k8s/repos.yaml: repo `c`"
        );
        assert_eq!(
            remove("b"),
            "removed `~/b` from ~/.obelisk-k8s/repos.yaml: repo `b`"
        );
        assert_eq!(repos_yaml(&root), "repos:\n# footer\n");
    }

    #[test]
    fn init_loads_what_was_there() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".obelisk-k8s");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("repos.yaml"), "repos: 1\n").unwrap();
        assert!(init(&root, Some(home.path())).is_err());
        // A file where the root goes.
        let file = home.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(init(&file, Some(home.path())).is_err());
    }

    #[test]
    fn removes_a_named_cluster_only_if_its_directory_holds_just_its_file() {
        let (home, root) = setup();
        std::fs::create_dir_all(root.join("clusters/other")).unwrap();
        std::fs::write(
            root.join("clusters/other/cluster.yaml"),
            "cluster:\n  name: named\n  k8s: k3d\n  location: local\n  managed: false\n",
        )
        .unwrap();
        std::fs::write(root.join("clusters/other/notes.txt"), "").unwrap();
        let error = remove_cluster(&root, Some(home.path()), "named")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("~/.obelisk-k8s/clusters/other holds more"),
            "{error}"
        );
    }

    #[test]
    fn dedupes_and_removes_by_place_not_spelling() {
        let (home, root) = setup();
        let h = Some(home.path());
        repo(home.path(), "work/c", "c");
        repo(home.path(), "dev/a", "a");
        add_repo(&root, h, "~/work/*").unwrap();
        assert_eq!(
            add_repo(&root, h, "../work/*").unwrap().message,
            "~/.obelisk-k8s/repos.yaml already lists that, as `~/work/*`"
        );
        add_repo(&root, h, "../dev/a").unwrap();
        assert_eq!(
            remove_repo(&root, h, "~/dev/a").unwrap().message,
            "removed `../dev/a` from ~/.obelisk-k8s/repos.yaml: repo `a`"
        );
        assert_eq!(
            remove_repo(&root, h, "../work/*").unwrap().message,
            "removed `~/work/*` from ~/.obelisk-k8s/repos.yaml: repo `c`"
        );
    }

    #[test]
    fn says_when_an_added_repo_wont_load() {
        let (home, root) = setup();
        let config = home.path().join("broken/.obelisk-k8s");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("repo.yaml"), "repo: {}\n").unwrap();
        let change = add_repo(&root, Some(home.path()), "~/broken").unwrap();
        assert_eq!(
            change.message,
            "added `~/broken` to ~/.obelisk-k8s/repos.yaml: (it won't load: see the warning)"
        );
        assert!(
            change.warnings[0].starts_with("~/broken: skipped: "),
            "{:?}",
            change.warnings
        );
    }

    #[test]
    fn removes_the_only_entry_and_its_comment() {
        let (home, root) = setup();
        repo(home.path(), "a", "a");
        std::fs::write(root.join("repos.yaml"), "repos:\n  # the a repo\n  - ~/a\n").unwrap();
        remove_repo(&root, Some(home.path()), "a").unwrap();
        assert_eq!(repos_yaml(&root), "repos:\n");
        assert_eq!(
            GlobalConfig::load(&root, Some(home.path())).unwrap().repos,
            []
        );
    }

    #[test]
    fn remove_with_no_repos_yaml_says_so() {
        let (home, root) = setup();
        std::fs::remove_file(root.join("repos.yaml")).unwrap();
        let error = remove_repo(&root, Some(home.path()), "a")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "no repo or entry `a` in ~/.obelisk-k8s/repos.yaml (entries: none)"
        );
    }
}
