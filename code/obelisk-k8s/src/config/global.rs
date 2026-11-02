//! The machine-wide config: `~/.obelisk-k8s/` by default (`--global-root`, `OBELISK_K8S_GLOBAL_ROOT`).
//!
//! ```text
//! ~/.obelisk-k8s/
//!   repos.yaml                    repos: [<path>, ...]  (relative to the global root, or `~/`, or absolute;
//!                                 a trailing `/*` means every repo in a dir)
//!   clusters/<dir>/cluster.yaml   cluster: { name, k8s, location, managed, obeliskd, kubectlCluster, kubectlContext }
//! ```
//!
//! A repo's name comes from its own `repo.yaml`; a cluster's defaults to its directory's name.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ObeliskK8sError;
use crate::config::repo::REPO_CONFIG_DIR;
use crate::config::repo::RepoConfig;
use crate::config::{check_unique, config_error, name_or_dir, read_entries};
use crate::yaml::{self, Spanned};

pub mod edit;

/// The env var that overrides the default global root.
pub const GLOBAL_ROOT_ENV: &str = "OBELISK_K8S_GLOBAL_ROOT";

/// `~/.obelisk-k8s`, or `None` without a home directory.
#[must_use]
pub fn default_global_root(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|home| home.join(".obelisk-k8s"))
}

/// The machine-wide config, loaded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalConfig {
    /// As the user should see it (`~/…` under the home directory).
    pub root: PathBuf,
    /// Sorted by name.
    pub repos: Vec<Repo>,
    /// Sorted by name.
    pub clusters: Vec<Cluster>,
    /// Things that look wrong but don't stop the load (e.g. a listed repo that won't load).
    pub warnings: Vec<String>,
}

/// A repo listed in `repos.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// From the repo's own `repo.yaml`.
    pub name: String,
    pub path: PathBuf,
    /// The `repos.yaml` entry it came from, as written.
    pub from: String,
    pub targets: Vec<RepoTarget>,
    pub units: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTarget {
    pub name: String,
    /// The cluster it names; a warning if it isn't in `clusters/`.
    pub cluster: String,
    /// Relative to the repo root.
    pub file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    pub name: String,
    /// As the user should see it.
    pub file: PathBuf,
    /// The real `clusters/<dir>/`.
    pub dir: PathBuf,
    pub k8s: K8s,
    pub location: Location,
    /// Whether obelisk-k8s creates and destroys it (`cluster up` / `down` / `destroy`; local k3d only).
    pub managed: bool,
    pub obeliskd: Option<Obeliskd>,
    pub kubectl_cluster: Option<String>,
    pub kubectl_context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum K8s {
    K3d,
    Talos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    Local,
    Remote,
}

/// As written in `cluster.yaml`.
impl fmt::Display for K8s {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::K3d => "k3d",
            Self::Talos => "talos",
        })
    }
}

/// As written in `cluster.yaml`.
impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Remote => "remote",
        })
    }
}

/// Where a managed cluster's addresses come from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Obeliskd {
    pub supernet: String,
    pub subnet: String,
}

impl GlobalConfig {
    /// The cluster named `name`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if there's none, naming the ones there are.
    pub fn cluster(&self, name: &str) -> Result<&Cluster, ObeliskK8sError> {
        self.clusters
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                let known: Vec<_> = self.clusters.iter().map(|c| c.name.as_str()).collect();
                let known = if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                };
                ObeliskK8sError::Usage(format!("no cluster `{name}` (there are: {known})"))
            })
    }

    /// Loads the global config at `root`. `home` expands `~/` (in `repos.yaml`, and when shown).
    ///
    /// A repo that won't load is a warning, so one broken repo doesn't hide the rest.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::NoGlobalConfig`] if `root` isn't a directory, and the other variants for a
    /// file that can't be read, isn't valid YAML of the right shape, or says something wrong
    /// (including two repos with the same name).
    pub fn load(root: &Path, home: Option<&Path>) -> Result<Self, ObeliskK8sError> {
        let shown = tilde(root, home);
        let root = &absolute_root(root, &shown)?;
        if !root.is_dir() {
            return Err(ObeliskK8sError::NoGlobalConfig(shown));
        }
        let mut warnings = Vec::new();
        let mut repos = load_repos(root, &shown, home, &mut warnings)?;
        let mut clusters = load_clusters(root, &shown, &mut warnings)?;
        for repo in &repos {
            for target in repo
                .targets
                .iter()
                .filter(|t| !clusters.iter().any(|c| c.name == t.cluster))
            {
                warnings.push(format!(
                    "{}/{}: cluster `{}` isn't in {}",
                    tilde(&repo.path, home).display(),
                    target.file.display(),
                    target.cluster,
                    shown.join("clusters").display()
                ));
            }
        }
        repos.sort_by(|a, b| a.name.cmp(&b.name));
        clusters.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            root: shown,
            repos,
            clusters,
            warnings,
        })
    }
}

/// Every repo `repos.yaml` lists (none without one), in the order listed, each path once.
fn load_repos(
    root: &Path,
    shown: &Path,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Repo>, ObeliskK8sError> {
    let repos_file = shown.join("repos.yaml");
    let mut repos: Vec<Repo> = Vec::new();
    for entry in read_repo_entries(root, shown)? {
        for path in expand(&repos_file, &entry, root, home, warnings)? {
            if repos.iter().any(|r| r.path == path) {
                continue;
            }
            let shown_path = tilde(&path, home);
            let config = match RepoConfig::load(&path) {
                Ok(config) => config,
                Err(error) => {
                    warnings.push(format!("{}: skipped: {error}", shown_path.display()));
                    continue;
                }
            };
            let prefix = shown_path.display();
            warnings.extend(config.warnings.iter().map(|w| format!("{prefix}/{w}")));
            let targets = config.targets.into_iter().map(|t| RepoTarget {
                name: t.name,
                cluster: t.cluster,
                file: t.file,
            });
            repos.push(Repo {
                name: config.name,
                from: entry.value.clone(),
                targets: targets.collect(),
                units: config.units.into_iter().map(|u| u.name).collect(),
                path,
            });
        }
    }
    let repo_files: Vec<PathBuf> = repos
        .iter()
        .map(|r| tilde(&r.path, home).join(REPO_CONFIG_DIR).join("repo.yaml"))
        .collect();
    let names = repos.iter().map(|r| r.name.as_str());
    check_unique(names.zip(repo_files.iter().map(PathBuf::as_path)), "repo")?;
    Ok(repos)
}

/// Every `clusters/<dir>/cluster.yaml`.
fn load_clusters(
    root: &Path,
    shown: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<Cluster>, ObeliskK8sError> {
    let mut clusters = Vec::new();
    for entry in read_entries(root, shown, "clusters", "cluster.yaml", warnings)? {
        let ClusterFile { cluster: c } = yaml::read_file(&entry.path, &entry.file)?;
        if let Some(obeliskd) = &c.obeliskd
            && !c.managed
        {
            return Err(config_error(
                &entry.file,
                Some(obeliskd.at),
                "`obeliskd` is only for a managed cluster (`managed: true`)",
            ));
        }
        if c.location == Location::Remote
            && c.kubectl_context.is_none()
            && c.kubectl_cluster.is_none()
        {
            warnings.push(format!(
                "{}: a remote cluster with no `kubectlContext` or `kubectlCluster`: obelisk-k8s won't guess its context",
                entry.file.display()
            ));
        }
        clusters.push(Cluster {
            name: name_or_dir(&entry, c.name)?,
            file: entry.file,
            dir: entry.dir,
            k8s: c.k8s,
            location: c.location,
            managed: c.managed,
            obeliskd: c.obeliskd.map(|o| o.value),
            kubectl_cluster: c.kubectl_cluster,
            kubectl_context: c.kubectl_context,
        });
    }
    check_unique(
        clusters.iter().map(|c| (c.name.as_str(), c.file.as_path())),
        "cluster",
    )?;
    Ok(clusters)
}

/// What `config global show` prints.
impl fmt::Display for GlobalConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "global root: {}", self.root.display())?;

        writeln!(
            f,
            "\nrepos:{}",
            if self.repos.is_empty() { " (none)" } else { "" }
        )?;
        for repo in &self.repos {
            writeln!(f, "  {}:  ({})", repo.name, repo.path.display())?;
            writeln!(f, "    from: {}", repo.from)?;
            let targets: Vec<String> = repo
                .targets
                .iter()
                .map(|t| format!("{} ({})", t.name, t.cluster))
                .collect();
            writeln!(f, "    targets: {}", list(&targets))?;
            writeln!(f, "    units: {}", list(&repo.units))?;
        }

        writeln!(
            f,
            "\nclusters:{}",
            if self.clusters.is_empty() {
                " (none)"
            } else {
                ""
            }
        )?;
        for c in &self.clusters {
            writeln!(f, "  {}:  ({})", c.name, c.file.display())?;
            let managed = if c.managed { "managed" } else { "not managed" };
            writeln!(f, "    {}, {}, {managed}", c.k8s, c.location)?;
            if let Some(o) = &c.obeliskd {
                writeln!(
                    f,
                    "    obeliskd: supernet {}, subnet {}",
                    o.supernet, o.subnet
                )?;
            }
            let unset = "(not set)";
            writeln!(
                f,
                "    kubectl cluster: {}",
                c.kubectl_cluster.as_deref().unwrap_or(unset)
            )?;
            writeln!(
                f,
                "    kubectl context: {}",
                c.kubectl_context.as_deref().unwrap_or(unset)
            )?;
        }
        Ok(())
    }
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "(none)".to_owned()
    } else {
        names.join(", ")
    }
}

/// `path` with the home directory shown as `~`.
#[must_use]
pub fn tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => Path::new("~").join(rest),
        None => path.to_owned(),
    }
}

/// `path` with `.` and `..` worked out (lexically: symlinks aren't followed).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // `/..` is `/`.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // Nothing left to cancel out (a relative path's leading `..`s): keep it.
                None | Some(Component::ParentDir) => out.push(".."),
                Some(_) => {
                    out.pop();
                }
            },
            other => out.push(other),
        }
    }
    out
}

/// The repo directories a `repos.yaml` entry names: one path, or with a trailing `/*`, every directory
/// in it that holds a `.obelisk-k8s/` (others are skipped quietly).
fn expand(
    file: &Path,
    entry: &Spanned<String>,
    root: &Path,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>, ObeliskK8sError> {
    let raw = entry.value.as_str();
    let EntryTarget { dir, wildcard } = resolve_entry(raw, root, home)
        .map_err(|message| config_error(file, Some(entry.at), message))?;
    if !wildcard {
        if is_repo(&dir) {
            return Ok(vec![dir]);
        }
        warnings.push(format!(
            "{}: skipped `{raw}`: no `{REPO_CONFIG_DIR}/` in {}",
            file.display(),
            tilde(&dir, home).display()
        ));
        return Ok(Vec::new());
    }
    let Some(found) = list_repos(&dir) else {
        warnings.push(format!(
            "{}: skipped `{raw}`: can't read {}",
            file.display(),
            dir.display()
        ));
        return Ok(Vec::new());
    };
    Ok(found)
}

/// Where a `repos.yaml` entry points: a directory, and whether it's every repo in it (`dir/*`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryTarget {
    dir: PathBuf,
    wildcard: bool,
}

/// Resolves an entry: `~/` from `home`, a relative path from `root` (absolute), normalized.
///
/// # Errors
///
/// A message naming the entry, for a misplaced `*` or a `~` with no home directory.
fn resolve_entry(raw: &str, root: &Path, home: Option<&Path>) -> Result<EntryTarget, String> {
    let (raw_dir, wildcard) = match raw.strip_suffix("/*") {
        Some(dir) => (dir, true),
        None => (raw, false),
    };
    if raw_dir.contains('*') {
        return Err(format!(
            "`{raw}`: `*` is only allowed as a whole last component (`dir/*`)"
        ));
    }
    let dir = if raw_dir == "~" || raw_dir.starts_with("~/") {
        let home = home.ok_or_else(|| format!("`{raw}`: no home directory to expand `~` with"))?;
        home.join(raw_dir.trim_start_matches('~').trim_start_matches('/'))
    } else {
        root.join(raw_dir)
    };
    Ok(EntryTarget {
        dir: normalize(&dir),
        wildcard,
    })
}

/// Whether `path` is a repo (holds a `.obelisk-k8s/`).
fn is_repo(path: &Path) -> bool {
    path.join(REPO_CONFIG_DIR).is_dir()
}

/// The repos directly in `dir`, sorted; `None` if it can't be read.
fn list_repos(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && is_repo(p))
        .collect();
    found.sort();
    Some(found)
}

/// `root`, absolute and normalized.
fn absolute_root(root: &Path, shown: &Path) -> Result<PathBuf, ObeliskK8sError> {
    let absolute = std::path::absolute(root).map_err(|source| ObeliskK8sError::Io {
        path: shown.to_owned(),
        source,
    })?;
    Ok(normalize(&absolute))
}

/// `repos.yaml`'s entries, as written (none without the file).
fn read_repo_entries(root: &Path, shown: &Path) -> Result<Vec<Spanned<String>>, ObeliskK8sError> {
    let path = root.join("repos.yaml");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let ReposFile { repos } = yaml::read_file(&path, &shown.join("repos.yaml"))?;
    Ok(repos.unwrap_or_default())
}

// The files, as written.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReposFile {
    repos: Option<Vec<Spanned<String>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterFile {
    cluster: ClusterSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClusterSection {
    name: Option<Spanned<String>>,
    k8s: K8s,
    location: Location,
    managed: bool,
    obeliskd: Option<Spanned<Obeliskd>>,
    kubectl_cluster: Option<String>,
    kubectl_context: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir standing in for `$HOME`, with the global root at `~/.obelisk-k8s`.
    struct Home(tempfile::TempDir);

    impl Home {
        fn new() -> Self {
            let home = Self(tempfile::tempdir().unwrap());
            std::fs::create_dir_all(home.root()).unwrap();
            home
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
        fn root(&self) -> PathBuf {
            self.path().join(".obelisk-k8s")
        }
        fn file(self, path: &str, text: &str) -> Self {
            let path = self.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            self
        }
        /// A repo at `~/<dir>` named `name`, with targets `t1` (cluster `c1`) and `t2` (cluster `nope`).
        fn repo(self, dir: &str, name: &str) -> Self {
            self.file(
                &format!("{dir}/.obelisk-k8s/repo.yaml"),
                &format!("repo:\n  name: {name}\n"),
            )
            .file(
                &format!("{dir}/.obelisk-k8s/targets/t1/target.yaml"),
                "target:\n  cluster: c1\n",
            )
            .file(
                &format!("{dir}/.obelisk-k8s/targets/t2/target.yaml"),
                "target:\n  cluster: nope\n",
            )
            .file(
                &format!("{dir}/.obelisk-k8s/units/app/unit.yaml"),
                "unit: {}\n",
            )
        }
        fn load(&self) -> Result<GlobalConfig, ObeliskK8sError> {
            GlobalConfig::load(&self.root(), Some(self.path()))
        }
        fn error(&self) -> String {
            self.load().unwrap_err().to_string()
        }
    }

    const CLUSTER: &str = "cluster:\n  k8s: k3d\n  location: local\n  managed: true\n  obeliskd: { supernet: s, subnet: n }\n";

    #[test]
    fn loads_repos_and_clusters() {
        let home = Home::new()
            .repo("dev/one", "one")
            .repo("work/two", "two")
            .repo("work/three", "three")
            .file("work/not-a-repo/README.md", "")
            .file(
                ".obelisk-k8s/repos.yaml",
                "repos:\n  - ~/dev/one\n  - \"~/work/*\"\n  - ~/work/two\n",
            )
            .file(".obelisk-k8s/clusters/c1/cluster.yaml", CLUSTER);
        let config = home.load().unwrap();
        assert_eq!(config.root, Path::new("~/.obelisk-k8s"));
        let names: Vec<_> = config.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["one", "three", "two"]);
        assert_eq!(config.repos[0].path, home.path().join("dev/one"));
        assert_eq!(config.repos[1].from, "~/work/*");
        assert_eq!(
            config.repos[2].from, "~/work/*",
            "listed twice: kept once, from its first entry"
        );
        assert_eq!(config.repos[0].units, ["app"]);
        assert_eq!(config.repos[0].targets[1].cluster, "nope");

        let c = &config.clusters[0];
        assert_eq!(
            (c.name.as_str(), c.k8s, c.location, c.managed),
            ("c1", K8s::K3d, Location::Local, true)
        );
        assert_eq!(c.obeliskd.as_ref().unwrap().subnet, "n");
        assert_eq!(c.file, Path::new("~/.obelisk-k8s/clusters/c1/cluster.yaml"));

        // Every repo's t2 names a cluster that isn't there.
        assert_eq!(config.warnings.len(), 3, "{:?}", config.warnings);
        assert_eq!(
            config.warnings[0],
            "~/dev/one/.obelisk-k8s/targets/t2/target.yaml: cluster `nope` isn't in ~/.obelisk-k8s/clusters"
        );
    }

    #[test]
    fn everything_but_the_root_is_optional() {
        let config = Home::new().load().unwrap();
        assert_eq!((config.repos.len(), config.clusters.len()), (0, 0));
        assert_eq!(config.warnings, Vec::<String>::new());
    }

    #[test]
    fn needs_the_root() {
        let home = Home::new();
        std::fs::remove_dir(home.root()).unwrap();
        assert!(
            matches!(home.load(), Err(ObeliskK8sError::NoGlobalConfig(p)) if p == Path::new("~/.obelisk-k8s"))
        );
    }

    #[test]
    fn rejects_bad_repo_paths() {
        let error = Home::new()
            .file(".obelisk-k8s/repos.yaml", "repos:\n  - \"~/a/*/b\"\n")
            .error();
        assert!(
            error.contains(
                "repos.yaml:2:5: `~/a/*/b`: `*` is only allowed as a whole last component"
            ),
            "{error}"
        );
    }

    #[test]
    fn relative_repo_paths_resolve_from_the_global_root() {
        let home = Home::new()
            .repo("dev/one", "one")
            .repo(".obelisk-k8s/inside", "inside")
            .repo("work/two", "two")
            .file(
                ".obelisk-k8s/repos.yaml",
                "repos: [../dev/one, inside, ./inside, \"../work/*\"]\n",
            );
        let config = home.load().unwrap();
        let paths: Vec<_> = config
            .repos
            .iter()
            .map(|r| (r.name.as_str(), r.path.clone()))
            .collect();
        assert_eq!(
            paths,
            [
                ("inside", home.root().join("inside")),
                ("one", home.path().join("dev/one")),
                ("two", home.path().join("work/two")),
            ]
        );
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(normalize(Path::new("/a/b/../../..")), Path::new("/"));
        assert_eq!(normalize(Path::new("../x")), Path::new("../x"));
        assert_eq!(normalize(Path::new("../../x/..")), Path::new("../.."));
        assert_eq!(normalize(Path::new("a/../../b")), Path::new("../b"));
    }

    #[test]
    fn rejects_duplicate_repo_names() {
        let error = Home::new()
            .repo("a", "same")
            .repo("b", "same")
            .file(".obelisk-k8s/repos.yaml", "repos: [~/a, ~/b]\n")
            .error();
        assert_eq!(
            error,
            "~/b/.obelisk-k8s/repo.yaml: repo name `same` is already used by ~/a/.obelisk-k8s/repo.yaml"
        );
    }

    #[test]
    fn warns_about_repos_that_are_missing_or_broken() {
        let config = Home::new()
            .file("broken/.obelisk-k8s/repo.yaml", "repo: {}\n")
            .file(
                ".obelisk-k8s/repos.yaml",
                "repos: [~/missing, ~/broken, \"~/nowhere/*\"]\n",
            )
            .load()
            .unwrap();
        assert_eq!(config.repos, []);
        let w = &config.warnings;
        assert_eq!(
            w[0],
            "~/.obelisk-k8s/repos.yaml: skipped `~/missing`: no `.obelisk-k8s/` in ~/missing"
        );
        assert!(w[1].starts_with("~/broken: skipped: "), "{w:?}");
        assert!(
            w[2].starts_with("~/.obelisk-k8s/repos.yaml: skipped `~/nowhere/*`: can't read "),
            "{w:?}"
        );
    }

    #[test]
    fn checks_clusters() {
        let error = Home::new()
            .file(
                ".obelisk-k8s/clusters/c/cluster.yaml",
                "cluster:\n  k8s: k3d\n  location: local\n  managed: false\n  obeliskd: { supernet: s, subnet: n }\n",
            )
            .error();
        assert_eq!(
            error,
            "~/.obelisk-k8s/clusters/c/cluster.yaml:5:13: `obeliskd` is only for a managed cluster (`managed: true`)"
        );
        let error = Home::new()
            .file(
                ".obelisk-k8s/clusters/c/cluster.yaml",
                "cluster:\n  k8s: minikube\n  location: local\n  managed: false\n",
            )
            .error();
        assert!(error.contains("minikube"), "{error}");

        let config = Home::new()
            .file(
                ".obelisk-k8s/clusters/r/cluster.yaml",
                "cluster:\n  name: remote-1\n  k8s: talos\n  location: remote\n  managed: false\n",
            )
            .load()
            .unwrap();
        assert_eq!(config.clusters[0].name, "remote-1");
        assert_eq!(
            config.warnings,
            [
                "~/.obelisk-k8s/clusters/r/cluster.yaml: a remote cluster with no `kubectlContext` or `kubectlCluster`: obelisk-k8s won't guess its context"
            ]
        );
    }

    #[test]
    fn renders() {
        let config = Home::new()
            .repo("dev/one", "one")
            .file(".obelisk-k8s/repos.yaml", "repos: [~/dev/one]\n")
            .file(".obelisk-k8s/clusters/c1/cluster.yaml", CLUSTER)
            .load()
            .unwrap();
        let text = config.to_string();
        let path = config.repos[0].path.display();
        assert_eq!(
            text,
            format!(
                "global root: ~/.obelisk-k8s\n\
                 \nrepos:\n  one:  ({path})\n    from: ~/dev/one\n    targets: t1 (c1), t2 (nope)\n    units: app\n\
                 \nclusters:\n  c1:  (~/.obelisk-k8s/clusters/c1/cluster.yaml)\n    k3d, local, managed\n    obeliskd: supernet s, subnet n\n    kubectl cluster: (not set)\n    kubectl context: (not set)\n"
            )
        );
    }
}
