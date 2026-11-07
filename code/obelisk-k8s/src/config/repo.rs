//! A repo's own config: `.obelisk-k8s/` at the repo root.
//!
//! ```text
//! .obelisk-k8s/
//!   repo.yaml                 repo: { name, helmRepos, build }
//!   targets/<name>/target.yaml target: { cluster, release: { name, namespace, version }, ephemeral }
//!   units/<name>/unit.yaml     unit: { targets, release: { name, namespace, version }, build }
//! ```
//!
//! `build` is the path to a unit's build file: a Go template that renders, per target, the phases that
//! make its values and manifests (see `config::build`). A unit's own `build` wins over the repo's; a
//! unit with neither is an error. It's interpolated (`${{obelisk:target}}`, `${{obelisk:unit}}`, and
//! `${{obelisk:path:unit}}`, the unit's directory: `./units/<name>`), and must start with `./` (from
//! `.obelisk-k8s/`), `/`, or `${{obelisk:path:unit}}`.
//!
//! `configPaths`, `generators` and `templates` are gone (a build file's phases list values and
//! manifests now): any of them is an error that says so.
//!
//! A target or unit is named by its directory (which must be a valid name): `name:` is gone, and an
//! error that says so. `null` and missing mean the same thing everywhere.

use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::ObeliskK8sError;
use crate::config::{check_name, config_error, dir_name, read_entries};
use crate::interp::{InterpString, StringPart, Var};
use crate::yaml::{self, Spanned};

/// The directory, at the repo root, that holds a repo's config.
pub const REPO_CONFIG_DIR: &str = ".obelisk-k8s";

/// `repo.yaml`, relative to the repo root.
pub const REPO_FILE: &str = ".obelisk-k8s/repo.yaml";

/// A repo's config, loaded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub name: String,
    /// Helm repo name → URL, in file order.
    pub helm_repos: IndexMap<String, String>,
    /// The build file of every unit without its own.
    pub build: Option<InterpString>,
    /// Sorted by name.
    pub targets: Vec<Target>,
    /// Sorted by name.
    pub units: Vec<Unit>,
    /// Things that look wrong but don't stop the load (e.g. a stray file in `units/`), in path order.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    /// Relative to the repo root, e.g. `.obelisk-k8s/targets/prod/target.yaml`.
    pub file: PathBuf,
    /// The name of a cluster in the global config (not checked yet).
    pub cluster: String,
    pub release: Release,
    /// Whether it's a stack that runs many times side by side, each copy named on the command line
    /// (`--ephemeral-name`). Missing or null means no.
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    /// Relative to the repo root, e.g. `.obelisk-k8s/units/my-app/unit.yaml`.
    pub file: PathBuf,
    /// Its own build file; `None` takes the repo's (and one of them has one).
    pub build: Option<InterpString>,
    pub targets: UnitTargets,
    pub release: Release,
}

impl Unit {
    /// `${{obelisk:path:unit}}`: its directory, from `.obelisk-k8s/` (`./units/my-app`); `None` if
    /// that isn't UTF-8.
    #[must_use]
    pub fn path(&self) -> Option<String> {
        let dir = self.file.parent()?.strip_prefix(REPO_CONFIG_DIR).ok()?;
        Some(format!("./{}", dir.to_str()?))
    }
}

/// What a target or unit says about the Helm release its units become: `release: { name, namespace,
/// version }`, each optional and interpolated. Like every value in these files, a plain scalar is read
/// as its text (`name: 123` is `"123"`). Agreed on, with what values sources set, after a case's
/// values phases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub name: Option<InterpString>,
    pub namespace: Option<InterpString>,
    /// The release's version (`obelisk.release.version`): the chart's `version` and `appVersion`.
    pub version: Option<InterpString>,
}

/// Which targets a unit deploys to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitTargets {
    All,
    /// In the order written, no duplicates; each names a target in the repo. Empty means none.
    Only(Vec<String>),
}

impl RepoConfig {
    /// Loads `.obelisk-k8s/` from `repo_root`. Stops at the first error.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::NoRepoConfig`] if `repo_root` has no `.obelisk-k8s/`, and the other variants
    /// for a file that can't be read, isn't valid YAML of the right shape, or says something wrong.
    pub fn load(repo_root: &Path) -> Result<Self, ObeliskK8sError> {
        let dir = repo_root.join(REPO_CONFIG_DIR);
        if !dir.is_dir() {
            return Err(ObeliskK8sError::NoRepoConfig);
        }
        let shown = Path::new(REPO_CONFIG_DIR);
        let mut warnings = Vec::new();

        let repo_file = Path::new(REPO_FILE);
        let RepoFile { repo } = yaml::read_file(&repo_root.join(REPO_FILE), repo_file)?;
        check_name(repo_file, &repo.name)?;
        check_gone(
            repo_file,
            repo.config_paths,
            repo.generators,
            repo.templates,
        )?;
        let helm_repos = repo.helm_repos.unwrap_or_default();
        let build = check_build(repo_file, repo.build)?;

        let mut targets = Vec::new();
        for entry in read_entries(&dir, shown, "targets", "target.yaml", &mut warnings)? {
            let TargetFile { target } = yaml::read_file(&entry.path, &entry.file)?;
            check_no_name(&entry.file, target.name, "a target", "targets")?;
            check_no_version(&entry.file, target.version)?;
            check_gone(
                &entry.file,
                target.config_paths,
                target.generators,
                target.templates,
            )?;
            targets.push(Target {
                name: dir_name(&entry, "rename it")?,
                cluster: target.cluster,
                release: target.release.unwrap_or_default(),
                ephemeral: target.ephemeral.unwrap_or(false),
                file: entry.file,
            });
        }

        let mut units = Vec::new();
        for entry in read_entries(&dir, shown, "units", "unit.yaml", &mut warnings)? {
            let UnitFile { unit } = yaml::read_file(&entry.path, &entry.file)?;
            check_no_name(&entry.file, unit.name, "a unit", "units")?;
            check_no_version(&entry.file, unit.version)?;
            check_gone(
                &entry.file,
                unit.config_paths,
                unit.generators,
                unit.templates,
            )?;
            let name = dir_name(&entry, "rename it")?;
            let unit_build = check_build(&entry.file, unit.build)?;
            if unit_build.is_none() && build.is_none() {
                return Err(config_error(
                    &entry.file,
                    None,
                    format!(
                        "unit `{name}` has no build file: set `build` here, or in `{REPO_FILE}` for every unit"
                    ),
                ));
            }
            units.push(Unit {
                name,
                build: unit_build,
                targets: check_unit_targets(&entry.file, unit.targets, &targets)?,
                release: unit.release.unwrap_or_default(),
                file: entry.file,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        units.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            name: repo.name.value,
            helm_repos,
            build,
            targets,
            units,
            warnings,
        })
    }

    /// The targets `unit` deploys to, in the repo's target order.
    #[must_use]
    pub fn unit_target_names<'a>(&'a self, unit: &'a Unit) -> Vec<&'a str> {
        let names = self.targets.iter().map(|t| t.name.as_str());
        match &unit.targets {
            UnitTargets::All => names.collect(),
            UnitTargets::Only(only) => names.filter(|t| only.iter().any(|o| o == t)).collect(),
        }
    }

    /// `unit`'s build file, as written, and the file that says so (its own `unit.yaml`, else
    /// `repo.yaml`).
    #[must_use]
    pub fn build_of<'a>(&'a self, unit: &'a Unit) -> (&'a InterpString, &'a Path) {
        match (&unit.build, &self.build) {
            (Some(own), _) => (own, &unit.file),
            // Checked at load: a unit without its own has the repo's.
            (None, Some(repo)) => (repo, Path::new(REPO_FILE)),
            (None, None) => unreachable!("unit `{}` has no build file", unit.name),
        }
    }
}

/// What `config repo show` prints.
impl fmt::Display for RepoConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "repo: {}", self.name)?;
        writeln!(f, "helm repos:{}", none_if(self.helm_repos.is_empty()))?;
        for (name, url) in &self.helm_repos {
            writeln!(f, "  {name}: {url}")?;
        }
        match &self.build {
            Some(build) => writeln!(f, "build: {build}")?,
            None => writeln!(f, "build: (none)")?,
        }

        writeln!(f, "\ntargets:{}", none_if(self.targets.is_empty()))?;
        for target in &self.targets {
            writeln!(f, "  {}:  ({})", target.name, target.file.display())?;
            writeln!(f, "    cluster: {}", target.cluster)?;
            if target.ephemeral {
                writeln!(f, "    ephemeral: yes")?;
            }
        }

        writeln!(f, "\nunits:{}", none_if(self.units.is_empty()))?;
        for unit in &self.units {
            writeln!(f, "  {}:  ({})", unit.name, unit.file.display())?;
            let names = self.unit_target_names(unit);
            let names = if names.is_empty() {
                "(none)".to_owned()
            } else {
                names.join(", ")
            };
            match unit.targets {
                UnitTargets::All => writeln!(f, "    targets: all: {names}")?,
                UnitTargets::Only(_) => writeln!(f, "    targets: {names}")?,
            }
            let (build, _) = self.build_of(unit);
            let whose = if unit.build.is_some() {
                ""
            } else {
                "  (the repo's)"
            };
            writeln!(f, "    build: {build}{whose}")?;
        }
        Ok(())
    }
}

fn none_if(empty: bool) -> &'static str {
    if empty { " (none)" } else { "" }
}

// The files, as written. serde reads a missing `Option` field as `None`, and serde-saphyr reads `null`
// as `None` too.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoFile {
    repo: RepoSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RepoSection {
    name: Spanned<String>,
    helm_repos: Option<IndexMap<String, String>>,
    build: Option<Spanned<InterpString>>,
    /// Gone (a build file's phases list values and manifests): only read to say so.
    config_paths: Option<Spanned<IgnoredAny>>,
    generators: Option<Spanned<IgnoredAny>>,
    templates: Option<Spanned<IgnoredAny>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    target: TargetSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TargetSection {
    /// Gone (a target is named by its directory): only read to say so.
    name: Option<Spanned<IgnoredAny>>,
    cluster: String,
    release: Option<Release>,
    /// Moved to `release.version`: only read to say so.
    version: Option<Spanned<IgnoredAny>>,
    ephemeral: Option<bool>,
    /// Gone (a build file's phases list values and manifests): only read to say so.
    config_paths: Option<Spanned<IgnoredAny>>,
    generators: Option<Spanned<IgnoredAny>>,
    templates: Option<Spanned<IgnoredAny>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitFile {
    unit: UnitSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UnitSection {
    /// Gone (a unit is named by its directory): only read to say so.
    name: Option<Spanned<IgnoredAny>>,
    targets: Option<Vec<Spanned<String>>>,
    release: Option<Release>,
    build: Option<Spanned<InterpString>>,
    /// Moved to `release.version`: only read to say so.
    version: Option<Spanned<IgnoredAny>>,
    /// Gone (a build file's phases list values and manifests): only read to say so.
    config_paths: Option<Spanned<IgnoredAny>>,
    generators: Option<Spanned<IgnoredAny>>,
    templates: Option<Spanned<IgnoredAny>>,
}

/// A `name` (gone: `what`, e.g. "a unit", is named by its directory in `kind/`) is an error that says
/// so.
fn check_no_name(
    file: &Path,
    name: Option<Spanned<IgnoredAny>>,
    what: &str,
    kind: &str,
) -> Result<(), ObeliskK8sError> {
    match name {
        Some(name) => Err(config_error(
            file,
            Some(name.at),
            format!("`name` is gone: {what} is named by its directory (`{kind}/<name>/`)"),
        )),
        None => Ok(()),
    }
}

/// A top-level `version` (now `release.version`) is an error that says where it went.
fn check_no_version(
    file: &Path,
    version: Option<Spanned<IgnoredAny>>,
) -> Result<(), ObeliskK8sError> {
    match version {
        Some(version) => Err(config_error(
            file,
            Some(version.at),
            "`version` is now `release.version`: move it under `release:`",
        )),
        None => Ok(()),
    }
}

/// `configPaths`, `generators` and `templates` (gone: a build file's phases list values and
/// manifests now) are each an error that says where they went.
fn check_gone(
    file: &Path,
    config_paths: Option<Spanned<IgnoredAny>>,
    generators: Option<Spanned<IgnoredAny>>,
    templates: Option<Spanned<IgnoredAny>>,
) -> Result<(), ObeliskK8sError> {
    let gone = [
        ("configPaths", config_paths, "list values"),
        ("generators", generators, "list them as `values` sources"),
        ("templates", templates, "list them as `manifests` sources"),
    ];
    for (key, given, instead) in gone {
        if let Some(given) = given {
            return Err(config_error(
                file,
                Some(given.at),
                format!("`{key}` is gone: {instead} in a build file's phases (`build:`)"),
            ));
        }
    }
    Ok(())
}

/// A build path: it must start with `./` (from `.obelisk-k8s/`), `/`, or `${{obelisk:path:unit}}`
/// (which is `./…`).
fn check_build(
    file: &Path,
    build: Option<Spanned<InterpString>>,
) -> Result<Option<InterpString>, ObeliskK8sError> {
    let Some(build) = build else {
        return Ok(None);
    };
    let ok = match build.value.parts().first() {
        Some(StringPart::Var(Var::PathUnit)) => true,
        Some(StringPart::Text(text)) => text.starts_with("./") || text.starts_with('/'),
        _ => false,
    };
    if !ok {
        return Err(config_error(
            file,
            Some(build.at),
            format!(
                "build path `{}` must start with `./` (from `{REPO_CONFIG_DIR}/`), `/`, or `${{{{obelisk:path:unit}}}}`",
                build.value
            ),
        ));
    }
    Ok(Some(build.value))
}

fn check_unit_targets(
    file: &Path,
    names: Option<Vec<Spanned<String>>>,
    targets: &[Target],
) -> Result<UnitTargets, ObeliskK8sError> {
    let Some(names) = names else {
        return Ok(UnitTargets::All);
    };
    for (i, name) in names.iter().enumerate() {
        if !targets.iter().any(|t| t.name == name.value) {
            let known = targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>();
            let known = if known.is_empty() {
                "no targets".to_owned()
            } else {
                known.join(", ")
            };
            return Err(config_error(
                file,
                Some(name.at),
                format!("unknown target `{}` (the repo has: {known})", name.value),
            ));
        }
        if names[..i].iter().any(|earlier| earlier.value == name.value) {
            return Err(config_error(
                file,
                Some(name.at),
                format!("target `{}` is listed twice", name.value),
            ));
        }
    }
    Ok(UnitTargets::Only(
        names.into_iter().map(|n| n.value).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NAME_RULE, is_valid_name};

    struct Repo(tempfile::TempDir);

    impl Repo {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn file(self, path: &str, text: &str) -> Self {
            let path = self.0.path().join(REPO_CONFIG_DIR).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            self
        }
        fn load(&self) -> Result<RepoConfig, ObeliskK8sError> {
            RepoConfig::load(self.0.path())
        }
        fn error(&self) -> String {
            self.load().unwrap_err().to_string()
        }
    }

    /// A repo whose units take its build file.
    fn base() -> Repo {
        Repo::new().file(
            "repo.yaml",
            "repo:\n  name: repo-1\n  build: ./build.yaml.gotmpl\n",
        )
    }

    fn names<T>(items: &[T], name: impl Fn(&T) -> &str) -> Vec<&str> {
        items.iter().map(name).collect()
    }

    #[test]
    fn loads_a_full_repo() {
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: repo-1\n  helmRepos:\n    ex1: https://abc.com/helm\n  build: \"./builds/${{obelisk:unit}}.yaml.gotmpl\"\n",
            )
            .file("targets/prod/target.yaml", "target:\n  cluster: homelab-1\n")
            .file("targets/local/target.yaml", "target:\n  cluster: k3d-1\n")
            .file("units/my-app/unit.yaml", "unit:\n  targets: null\n  build: \"${{obelisk:path:unit}}/build.yaml.gotmpl\"\n")
            .file("units/db/unit.yaml", "unit:\n  targets: [prod, local]\n")
            .load()
            .unwrap();

        assert_eq!(repo.name, "repo-1");
        assert_eq!(repo.helm_repos["ex1"], "https://abc.com/helm");
        assert_eq!(
            repo.build.as_ref().unwrap().raw(),
            "./builds/${{obelisk:unit}}.yaml.gotmpl"
        );
        assert_eq!(names(&repo.targets, |t| &t.name), ["local", "prod"]);
        assert_eq!(names(&repo.units, |u| &u.name), ["db", "my-app"]);
        assert_eq!(repo.units[1].targets, UnitTargets::All);
        assert_eq!(
            repo.units[0].targets,
            UnitTargets::Only(vec!["prod".into(), "local".into()])
        );
        assert_eq!(repo.unit_target_names(&repo.units[0]), ["local", "prod"]);
        assert_eq!(
            repo.units[0].file,
            Path::new(".obelisk-k8s/units/db/unit.yaml")
        );
        assert_eq!(repo.warnings, Vec::<String>::new());
        // A unit's own build file wins over the repo's.
        let (build, from) = repo.build_of(&repo.units[1]);
        assert_eq!(build.raw(), "${{obelisk:path:unit}}/build.yaml.gotmpl");
        assert_eq!(from, Path::new(".obelisk-k8s/units/my-app/unit.yaml"));
        let (build, from) = repo.build_of(&repo.units[0]);
        assert_eq!(build.raw(), "./builds/${{obelisk:unit}}.yaml.gotmpl");
        assert_eq!(from, Path::new(REPO_FILE));
        // `${{obelisk:path:unit}}` is its directory, from `.obelisk-k8s/`.
        assert_eq!(repo.units[1].path().as_deref(), Some("./units/my-app"));
    }

    #[test]
    fn a_unit_needs_a_build_file() {
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n")
            .file("units/app/unit.yaml", "unit: {}\n")
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/units/app/unit.yaml: unit `app` has no build file: set `build` here, or in `.obelisk-k8s/repo.yaml` for every unit"
        );
        // Its own is enough.
        let repo = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n  build: null\n")
            .file("units/app/unit.yaml", "unit:\n  build: /abs/build.gotmpl\n")
            .load()
            .unwrap();
        assert_eq!(repo.build, None);
        assert_eq!(repo.build_of(&repo.units[0]).0.raw(), "/abs/build.gotmpl");
    }

    #[test]
    fn checks_build_paths() {
        let error = |file: &str, text: &str| base().file(file, text).error();
        let rule = "must start with `./` (from `.obelisk-k8s/`), `/`, or `${{obelisk:path:unit}}`";
        assert_eq!(
            error("units/app/unit.yaml", "unit:\n  build: build.gotmpl\n"),
            format!(".obelisk-k8s/units/app/unit.yaml:2:10: build path `build.gotmpl` {rule}")
        );
        assert_eq!(
            error(
                "repo.yaml",
                "repo:\n  name: r\n  build: \"${{obelisk:unit}}/b\"\n"
            ),
            format!(".obelisk-k8s/repo.yaml:3:10: build path `${{{{obelisk:unit}}}}/b` {rule}")
        );
        let e = error(
            "units/app/unit.yaml",
            "unit:\n  build: \"./${{obelisk:nope}}\"\n",
        );
        assert!(e.contains("unknown var `obelisk:nope`"), "{e}");
        // Targets have none.
        let e = error(
            "targets/t/target.yaml",
            "target:\n  cluster: c\n  build: ./b\n",
        );
        assert!(e.contains("build"), "{e}");
    }

    #[test]
    fn says_where_config_paths_generators_and_templates_went() {
        for (file, text, expected) in [
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./a.yaml]\n",
                ".obelisk-k8s/repo.yaml:3:16: `configPaths` is gone: list values in a build file's phases (`build:`)",
            ),
            (
                "targets/t/target.yaml",
                "target:\n  cluster: c\n  generators: []\n",
                ".obelisk-k8s/targets/t/target.yaml:3:15: `generators` is gone: list them as `values` sources in a build file's phases (`build:`)",
            ),
            (
                "units/app/unit.yaml",
                "unit:\n  templates: []\n",
                ".obelisk-k8s/units/app/unit.yaml:2:14: `templates` is gone: list them as `manifests` sources in a build file's phases (`build:`)",
            ),
        ] {
            assert_eq!(base().file(file, text).error(), expected, "{text:?}");
        }
    }

    #[test]
    fn parses_optional_release_fields() {
        let repo = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release:\n    namespace: \"mc-${{obelisk:target}}\"\n",
            )
            .file(
                "units/app/unit.yaml",
                "unit:\n  release: { name: app, namespace: null }\n",
            )
            .file("units/other/unit.yaml", "unit: {}\n")
            .load()
            .unwrap();
        let ns = repo.targets[0].release.namespace.as_ref().unwrap();
        assert_eq!(ns.raw(), "mc-${{obelisk:target}}");
        assert_eq!(repo.targets[0].release.name, None);
        assert_eq!(repo.units[0].release.name.as_ref().unwrap().raw(), "app");
        assert_eq!(repo.units[0].release.namespace, None);
        assert_eq!(repo.units[1].release, Release::default());

        let error = base()
            .file("units/app/unit.yaml", "unit:\n  release: { nmae: app }\n")
            .error();
        assert!(error.contains("nmae"), "{error}");
        let error = base()
            .file(
                "targets/t/target.yaml",
                "target:\n  cluster: c\n  release: { nmaespace: x }\n",
            )
            .error();
        assert!(error.contains("nmaespace"), "{error}");
        let error = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  release: { name: \"${{obelisk:untis}}\" }\n",
            )
            .error();
        assert!(error.contains("unknown var `obelisk:untis`"), "{error}");
    }

    #[test]
    fn parses_the_release_version() {
        let repo = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { version: \"0.0.0-${{obelisk:target}}\" }\n",
            )
            .file(
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.0.0 }\n",
            )
            .load()
            .unwrap();
        let version = repo.targets[0].release.version.as_ref().unwrap();
        assert_eq!(version.raw(), "0.0.0-${{obelisk:target}}");
        assert_eq!(
            repo.units[0].release.version.as_ref().unwrap().raw(),
            "1.0.0"
        );
    }

    #[test]
    fn says_where_a_top_level_version_went() {
        let moved = "`version` is now `release.version`: move it under `release:`";
        let error = base()
            .file("units/app/unit.yaml", "unit:\n  version: 1.0.0\n")
            .error();
        assert_eq!(
            error,
            format!(".obelisk-k8s/units/app/unit.yaml:2:12: {moved}")
        );
        let error = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  version: [not, a, string]\n",
            )
            .error();
        assert_eq!(
            error,
            format!(".obelisk-k8s/targets/prod/target.yaml:3:12: {moved}")
        );
    }

    #[test]
    fn an_empty_release_and_plain_scalars() {
        let repo = base()
            .file(
                "targets/a/target.yaml",
                "target:\n  cluster: c\n  release: null\n",
            )
            .file(
                "targets/b/target.yaml",
                "target:\n  cluster: c\n  release: {}\n",
            )
            .file(
                "units/app/unit.yaml",
                "unit:\n  release: { name: 123, namespace: 0755 }\n",
            )
            .load()
            .unwrap();
        assert_eq!(repo.targets[0].release, Release::default());
        assert_eq!(repo.targets[1].release, Release::default());
        // Like every value in these files, a plain scalar is read as its text.
        let release = &repo.units[0].release;
        assert_eq!(release.name.as_ref().unwrap().raw(), "123");
        assert_eq!(release.namespace.as_ref().unwrap().raw(), "0755");
    }

    #[test]
    fn ephemeral_defaults_to_false() {
        let repo = base()
            .file("targets/a/target.yaml", "target:\n  cluster: c\n")
            .file(
                "targets/b/target.yaml",
                "target:\n  cluster: c\n  ephemeral: null\n",
            )
            .file(
                "targets/c/target.yaml",
                "target:\n  cluster: c\n  ephemeral: false\n",
            )
            .file(
                "targets/d/target.yaml",
                "target:\n  cluster: c\n  ephemeral: true\n",
            )
            .load()
            .unwrap();
        let ephemeral: Vec<bool> = repo.targets.iter().map(|t| t.ephemeral).collect();
        assert_eq!(ephemeral, [false, false, false, true]);
        assert!(repo.to_string().contains(
            "  d:  (.obelisk-k8s/targets/d/target.yaml)\n    cluster: c\n    ephemeral: yes\n"
        ));
        assert!(
            base()
                .file(
                    "targets/a/target.yaml",
                    "target:\n  cluster: c\n  ephemeral: maybe\n"
                )
                .load()
                .is_err()
        );
    }

    #[test]
    fn null_and_missing_are_the_same() {
        let repo = Repo::new()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  helmRepos: null\n  build: ./b\n",
            )
            .file(
                "targets/t/target.yaml",
                "target:\n  cluster: c\n  release: null\n",
            )
            .file("units/a/unit.yaml", "unit:\n")
            .file("units/b/unit.yaml", "unit: { build: null }\n")
            .load()
            .unwrap();
        assert!(repo.helm_repos.is_empty());
        assert_eq!(names(&repo.units, |u| &u.name), ["a", "b"]);
        for unit in &repo.units {
            assert_eq!(unit.targets, UnitTargets::All);
            assert_eq!(unit.build, None);
        }
    }

    #[test]
    fn an_empty_targets_list_means_none() {
        let repo = base()
            .file("targets/prod/target.yaml", "target:\n  cluster: c\n")
            .file("units/app/unit.yaml", "unit:\n  targets: []\n")
            .load()
            .unwrap();
        assert_eq!(repo.units[0].targets, UnitTargets::Only(vec![]));
        assert_eq!(repo.unit_target_names(&repo.units[0]), Vec::<&str>::new());
        assert!(repo.to_string().contains("    targets: (none)\n"), "{repo}");
    }

    #[test]
    fn targets_and_units_dirs_are_optional() {
        let repo = base().load().unwrap();
        assert_eq!(repo.targets, []);
        assert_eq!(repo.units, []);
    }

    #[test]
    fn needs_the_config_dir() {
        assert!(matches!(
            Repo::new().load(),
            Err(ObeliskK8sError::NoRepoConfig)
        ));
        let repo = Repo::new();
        std::fs::write(repo.0.path().join(REPO_CONFIG_DIR), "").unwrap();
        assert!(matches!(repo.load(), Err(ObeliskK8sError::NoRepoConfig)));
    }

    #[test]
    fn checks_the_repo_file() {
        let error = Repo::new().file("repo.yaml", "").error();
        assert!(error.starts_with(".obelisk-k8s/repo.yaml"), "{error}");
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  helmRepos: {}\n")
            .error();
        assert!(error.contains("name"), "{error}");
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  name: Repo\n")
            .error();
        assert_eq!(
            error,
            format!(".obelisk-k8s/repo.yaml:2:9: `Repo` isn't a valid name ({NAME_RULE})")
        );
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n  build: b.yaml\n")
            .error();
        assert!(
            error.starts_with(
                ".obelisk-k8s/repo.yaml:3:10: build path `b.yaml` must start with `./`"
            ),
            "{error}"
        );
    }

    #[test]
    fn checks_the_repo_file_before_the_others() {
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n  build: bad\n")
            .file("units/app/unit.yaml", "unit:\n  name: BAD\n")
            .error();
        assert!(error.starts_with(".obelisk-k8s/repo.yaml:"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_target_with_its_location() {
        let error = base()
            .file("targets/prod/target.yaml", "target:\n  cluster: c\n")
            .file(
                "units/app/unit.yaml",
                "unit:\n  targets:\n    - prod\n    - prdo\n",
            )
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/units/app/unit.yaml:4:7: unknown target `prdo` (the repo has: prod)"
        );
    }

    #[test]
    fn rejects_a_target_listed_twice() {
        let error = base()
            .file("targets/prod/target.yaml", "target:\n  cluster: c\n")
            .file("units/app/unit.yaml", "unit:\n  targets: [prod, prod]\n")
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/units/app/unit.yaml:2:19: target `prod` is listed twice"
        );
    }

    #[test]
    fn says_a_name_is_its_directory() {
        let error = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  name: code-app\n  targets: [nope]\n",
            )
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/units/app/unit.yaml:2:9: `name` is gone: a unit is named by its directory (`units/<name>/`)"
        );
        let error = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  name: [x]\n",
            )
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/targets/prod/target.yaml:3:9: `name` is gone: a target is named by its directory (`targets/<name>/`)"
        );
        // `null` is no name, as everywhere.
        let repo = base()
            .file("units/app/unit.yaml", "unit:\n  name: null\n")
            .load()
            .unwrap();
        assert_eq!(names(&repo.units, |u| &u.name), ["app"]);
    }

    #[test]
    fn yaml_errors_name_the_file_and_location() {
        let error = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  clustr: d\n",
            )
            .error();
        assert!(
            error.contains(".obelisk-k8s/targets/prod/target.yaml:3:3"),
            "{error}"
        );
        assert!(error.contains("clustr"), "{error}");
        assert!(!error.contains("<input>"), "{error}");
    }

    #[test]
    fn rejects_unknown_wrapper_keys() {
        let error = base()
            .file("units/app/unit.yaml", "unit: {}\nunits: {}\n")
            .error();
        assert!(
            error.contains(".obelisk-k8s/units/app/unit.yaml:2:1"),
            "{error}"
        );
    }

    #[test]
    fn checks_names() {
        let error = base().file("units/My_App/unit.yaml", "unit: {}\n").error();
        assert_eq!(
            error,
            format!(
                ".obelisk-k8s/units/My_App/unit.yaml: the directory name `My_App` isn't a valid name ({NAME_RULE}): rename it"
            )
        );
        let error = base()
            .file("targets/-prod/target.yaml", "target:\n  cluster: c\n")
            .error();
        assert!(error.contains("`-prod` isn't a valid name"), "{error}");
        assert!(is_valid_name(&"a".repeat(63)));
        assert!(!is_valid_name(&"a".repeat(64)));
        assert!(!is_valid_name(""));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_dir_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let named = base();
        let dir = named
            .0
            .path()
            .join(".obelisk-k8s/units")
            .join(std::ffi::OsStr::from_bytes(b"app-\xff"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("unit.yaml"), "unit: {}\n").unwrap();
        assert!(
            named
                .error()
                .ends_with("unit.yaml: the directory name isn't UTF-8: rename it")
        );
    }

    #[test]
    fn warns_about_stray_entries_and_skips_hidden_ones() {
        let repo = base()
            .file("units/README.md", "hi\n")
            .file("units/app/values.yaml", "a: 1\n")
            .file("units/.gitkeep", "")
            .file("targets/prod.yaml", "target:\n  cluster: c\n")
            .load()
            .unwrap();
        assert_eq!(repo.units, []);
        assert_eq!(
            repo.warnings,
            [
                ".obelisk-k8s/targets/prod.yaml: ignored: not a directory (expected targets/<name>/target.yaml)",
                ".obelisk-k8s/units/README.md: ignored: not a directory (expected units/<name>/unit.yaml)",
                ".obelisk-k8s/units/app: ignored: no `unit.yaml` in this directory",
            ]
        );
    }

    #[test]
    fn renders() {
        let repo = base()
            .file(
                "targets/prod/target.yaml",
                "target:\n  cluster: homelab-1\n",
            )
            .file("units/app/unit.yaml", "unit: {}\n")
            .file(
                "units/web/unit.yaml",
                "unit:\n  build: \"${{obelisk:path:unit}}/build.yaml.gotmpl\"\n",
            )
            .load()
            .unwrap();
        assert_eq!(
            repo.to_string(),
            "repo: repo-1\nhelm repos: (none)\nbuild: ./build.yaml.gotmpl\n\
             \ntargets:\n  prod:  (.obelisk-k8s/targets/prod/target.yaml)\n    cluster: homelab-1\n\
             \nunits:\n  app:  (.obelisk-k8s/units/app/unit.yaml)\n    targets: all: prod\n    build: ./build.yaml.gotmpl  (the repo's)\n\
             \x20 web:  (.obelisk-k8s/units/web/unit.yaml)\n    targets: all: prod\n    build: ${{obelisk:path:unit}}/build.yaml.gotmpl\n"
        );
    }
}
