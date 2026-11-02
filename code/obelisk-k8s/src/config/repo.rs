//! A repo's own config: `.obelisk-k8s/` at the repo root.
//!
//! ```text
//! .obelisk-k8s/
//!   repo.yaml                 repo: { name, helmRepos, configPaths, generators, templates }
//!   targets/<dir>/target.yaml target: { name, cluster, configPaths, generators, templates, release: { name, namespace }, version }
//!   units/<dir>/unit.yaml     unit: { name, configPaths, generators, templates, targets, release: { name, namespace }, version }
//! ```
//!
//! A target's or unit's name defaults to its directory's name. `null` and missing mean the same thing
//! everywhere.

use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::ObeliskK8sError;
use crate::config::{check_name, check_unique, config_error, name_or_dir, read_entries};
use crate::interp::InterpString;
use crate::yaml::{self, Spanned};

/// The directory, at the repo root, that holds a repo's config.
pub const REPO_CONFIG_DIR: &str = ".obelisk-k8s";

/// What generator and template paths are called in errors.
const GENERATOR_PATH: &str = "generator path";
const TEMPLATE_PATH: &str = "template path";

/// `repo.yaml`, relative to the repo root.
pub const REPO_FILE: &str = ".obelisk-k8s/repo.yaml";

/// A repo's config, loaded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub name: String,
    /// Helm repo name → URL, in file order.
    pub helm_repos: IndexMap<String, String>,
    pub config_paths: Vec<InterpString>,
    pub generators: Vec<RenderFile>,
    pub templates: Vec<RenderFile>,
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
    pub config_paths: Vec<InterpString>,
    pub generators: Vec<RenderFile>,
    pub templates: Vec<RenderFile>,
    pub release: Release,
    /// The version its units' Helm releases get (`obelisk.version`), interpolated. Parsed here, agreed
    /// on after generation.
    pub version: Option<InterpString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    /// Relative to the repo root, e.g. `.obelisk-k8s/units/my-app/unit.yaml`.
    pub file: PathBuf,
    pub config_paths: Vec<InterpString>,
    pub generators: Vec<RenderFile>,
    pub templates: Vec<RenderFile>,
    pub targets: UnitTargets,
    pub release: Release,
    /// The version its Helm release gets (`obelisk.version`), interpolated. Parsed here, agreed on after
    /// generation.
    pub version: Option<InterpString>,
}

/// A file obelisk-k8s renders: a generator (`generators:`, more values from the merged config values)
/// or a template (`templates:`, k8s resources from the generated values). Its path is like a config
/// path: it starts with `./`, is interpolated, and is skipped if it's missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderFile {
    pub kind: RenderKind,
    pub path: InterpString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderKind {
    /// A Go template, rendered by Helm.
    Gotmpl,
}

/// What a target or unit says about the Helm release its units become: `release: { name, namespace }`,
/// both optional and interpolated. Like every value in these files, a plain scalar is read as its text
/// (`name: 123` is `"123"`). Parsed, not used yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub name: Option<InterpString>,
    pub namespace: Option<InterpString>,
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
        let config_paths = check_config_paths(repo_file, repo.config_paths)?;
        let generators = check_render_files(repo_file, repo.generators, GENERATOR_PATH)?;
        let templates = check_render_files(repo_file, repo.templates, TEMPLATE_PATH)?;

        let mut targets = Vec::new();
        for entry in read_entries(&dir, shown, "targets", "target.yaml", &mut warnings)? {
            let TargetFile { target } = yaml::read_file(&entry.path, &entry.file)?;
            targets.push(Target {
                name: name_or_dir(&entry, target.name)?,
                config_paths: check_config_paths(&entry.file, target.config_paths)?,
                generators: check_render_files(&entry.file, target.generators, GENERATOR_PATH)?,
                templates: check_render_files(&entry.file, target.templates, TEMPLATE_PATH)?,
                cluster: target.cluster,
                release: target.release.unwrap_or_default(),
                version: target.version,
                file: entry.file,
            });
        }
        check_unique(
            targets.iter().map(|t| (t.name.as_str(), t.file.as_path())),
            "target",
        )?;

        let mut units = Vec::new();
        for entry in read_entries(&dir, shown, "units", "unit.yaml", &mut warnings)? {
            let UnitFile { unit } = yaml::read_file(&entry.path, &entry.file)?;
            units.push(Unit {
                name: name_or_dir(&entry, unit.name)?,
                config_paths: check_config_paths(&entry.file, unit.config_paths)?,
                generators: check_render_files(&entry.file, unit.generators, GENERATOR_PATH)?,
                templates: check_render_files(&entry.file, unit.templates, TEMPLATE_PATH)?,
                targets: check_unit_targets(&entry.file, unit.targets, &targets)?,
                release: unit.release.unwrap_or_default(),
                version: unit.version,
                file: entry.file,
            });
        }
        check_unique(
            units.iter().map(|u| (u.name.as_str(), u.file.as_path())),
            "unit",
        )?;

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        units.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            name: repo.name.value,
            helm_repos: repo.helm_repos.unwrap_or_default(),
            config_paths,
            generators,
            templates,
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
}

/// What `config repo show` prints.
impl fmt::Display for RepoConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "repo: {}", self.name)?;
        writeln!(f, "helm repos:{}", none_if(self.helm_repos.is_empty()))?;
        for (name, url) in &self.helm_repos {
            writeln!(f, "  {name}: {url}")?;
        }
        write_config_paths(f, "", &self.config_paths)?;

        writeln!(f, "\ntargets:{}", none_if(self.targets.is_empty()))?;
        for target in &self.targets {
            writeln!(f, "  {}:  ({})", target.name, target.file.display())?;
            writeln!(f, "    cluster: {}", target.cluster)?;
            write_config_paths(f, "    ", &target.config_paths)?;
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
            write_config_paths(f, "    ", &unit.config_paths)?;
        }
        Ok(())
    }
}

fn none_if(empty: bool) -> &'static str {
    if empty { " (none)" } else { "" }
}

fn write_config_paths(
    f: &mut fmt::Formatter<'_>,
    indent: &str,
    paths: &[InterpString],
) -> fmt::Result {
    writeln!(f, "{indent}config paths:{}", none_if(paths.is_empty()))?;
    for path in paths {
        writeln!(f, "{indent}  - {path}")?;
    }
    Ok(())
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
    config_paths: Option<Vec<Spanned<InterpString>>>,
    generators: Option<Vec<RenderFileIn>>,
    templates: Option<Vec<RenderFileIn>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    target: TargetSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TargetSection {
    name: Option<Spanned<String>>,
    cluster: String,
    config_paths: Option<Vec<Spanned<InterpString>>>,
    generators: Option<Vec<RenderFileIn>>,
    templates: Option<Vec<RenderFileIn>>,
    release: Option<Release>,
    version: Option<InterpString>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitFile {
    unit: UnitSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UnitSection {
    name: Option<Spanned<String>>,
    config_paths: Option<Vec<Spanned<InterpString>>>,
    generators: Option<Vec<RenderFileIn>>,
    templates: Option<Vec<RenderFileIn>>,
    targets: Option<Vec<Spanned<String>>>,
    release: Option<Release>,
    version: Option<InterpString>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderFileIn {
    #[serde(rename = "type")]
    kind: RenderKind,
    path: Spanned<InterpString>,
}

/// Every config path must start with `./` (it resolves from `.obelisk-k8s/`).
fn check_render_files(
    file: &Path,
    files: Option<Vec<RenderFileIn>>,
    what: &str,
) -> Result<Vec<RenderFile>, ObeliskK8sError> {
    let files = files.unwrap_or_default();
    let paths = files.iter().map(|f| f.path.clone()).collect();
    check_paths(file, Some(paths), what)?;
    Ok(files
        .into_iter()
        .map(|f| RenderFile {
            kind: f.kind,
            path: f.path.value,
        })
        .collect())
}

fn check_config_paths(
    file: &Path,
    paths: Option<Vec<Spanned<InterpString>>>,
) -> Result<Vec<InterpString>, ObeliskK8sError> {
    check_paths(file, paths, "config path")
}

/// Every such path must start with `./` (it resolves from `.obelisk-k8s/`). `what` names them.
fn check_paths(
    file: &Path,
    paths: Option<Vec<Spanned<InterpString>>>,
    what: &str,
) -> Result<Vec<InterpString>, ObeliskK8sError> {
    let paths = paths.unwrap_or_default();
    if let Some(bad) = paths.iter().find(|p| !p.value.raw().starts_with("./")) {
        return Err(config_error(
            file,
            Some(bad.at),
            format!(
                "{what} `{}` must start with `./` (paths resolve from `{REPO_CONFIG_DIR}/`)",
                bad.value
            ),
        ));
    }
    Ok(paths.into_iter().map(|p| p.value).collect())
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

    fn base() -> Repo {
        Repo::new().file("repo.yaml", "repo:\n  name: repo-1\n")
    }

    fn names<T>(items: &[T], name: impl Fn(&T) -> &str) -> Vec<&str> {
        items.iter().map(name).collect()
    }

    #[test]
    fn loads_a_full_repo() {
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: repo-1\n  helmRepos:\n    ex1: https://abc.com/helm\n  configPaths:\n    - \"./config/.global/values.yaml\"\n    - \"./config/${{obelisk:unit}}/values.yaml\"\n",
            )
            .file("targets/prod/target.yaml", "target:\n  cluster: homelab-1\n")
            .file("targets/local/target.yaml", "target:\n  name: null\n  cluster: k3d-1\n  configPaths: []\n")
            .file("units/my-app/unit.yaml", "unit:\n  name: code-app\n  targets: null\n")
            .file("units/db/unit.yaml", "unit:\n  targets: [prod, local]\n  configPaths: [\"./config/db.yaml\"]\n")
            .load()
            .unwrap();

        assert_eq!(repo.name, "repo-1");
        assert_eq!(repo.helm_repos["ex1"], "https://abc.com/helm");
        assert_eq!(
            repo.config_paths[1].raw(),
            "./config/${{obelisk:unit}}/values.yaml"
        );
        assert_eq!(names(&repo.targets, |t| &t.name), ["local", "prod"]);
        assert_eq!(names(&repo.units, |u| &u.name), ["code-app", "db"]);
        assert_eq!(repo.units[0].targets, UnitTargets::All);
        assert_eq!(
            repo.units[1].targets,
            UnitTargets::Only(vec!["prod".into(), "local".into()])
        );
        assert_eq!(repo.unit_target_names(&repo.units[1]), ["local", "prod"]);
        assert_eq!(
            repo.units[1].file,
            Path::new(".obelisk-k8s/units/db/unit.yaml")
        );
        assert_eq!(repo.warnings, Vec::<String>::new());
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
    fn loads_generators() {
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  generators: [{ type: gotmpl, path: ./g.gotmpl }]\n",
            )
            .file("units/app/unit.yaml", "unit:\n  generators: null\n")
            .load()
            .unwrap();
        assert_eq!(repo.generators[0].kind, RenderKind::Gotmpl);
        assert_eq!(repo.generators[0].path.raw(), "./g.gotmpl");
        assert_eq!(repo.units[0].generators, []);
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  templates: [{ type: gotmpl, path: ./t.gotmpl }]\n",
            )
            .load()
            .unwrap();
        assert_eq!(repo.templates[0].path.raw(), "./t.gotmpl");
        let e = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  templates: [{ type: gotmpl, path: t }]\n",
            )
            .error();
        assert!(e.contains("template path `t` must start with `./`"), "{e}");

        let error = |text: &str| base().file("units/app/unit.yaml", text).error();
        let e = error("unit:\n  generators: [{ type: gotmpl, path: g.gotmpl }]\n");
        assert_eq!(
            e,
            ".obelisk-k8s/units/app/unit.yaml:2:38: generator path `g.gotmpl` must start with `./` (paths resolve from `.obelisk-k8s/`)"
        );
        let e = error("unit:\n  generators: [{ type: helm, path: ./g }]\n");
        assert!(e.contains("helm") && e.contains("gotmpl"), "{e}");
        let e = error("unit:\n  generators: [{ type: gotmpl, path: ./g, extra: 1 }]\n");
        assert!(e.contains("extra"), "{e}");
        let e = error("unit:\n  generators: [{ path: ./g }]\n");
        assert!(e.contains("type"), "{e}");
    }

    #[test]
    fn null_and_missing_are_the_same() {
        let repo = Repo::new()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  helmRepos: null\n  configPaths: null\n",
            )
            .file(
                "targets/t/target.yaml",
                "target:\n  cluster: c\n  configPaths: null\n",
            )
            .file("units/a/unit.yaml", "unit:\n")
            .file("units/b/unit.yaml", "unit: {}\n")
            .load()
            .unwrap();
        assert!(repo.helm_repos.is_empty());
        assert_eq!(repo.config_paths, []);
        assert_eq!(names(&repo.units, |u| &u.name), ["a", "b"]);
        for unit in &repo.units {
            assert_eq!(unit.targets, UnitTargets::All);
            assert_eq!(unit.config_paths, []);
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
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./a.yaml, b.yaml]\n",
            )
            .error();
        assert!(
            error.starts_with(
                ".obelisk-k8s/repo.yaml:3:27: config path `b.yaml` must start with `./`"
            ),
            "{error}"
        );
    }

    #[test]
    fn checks_the_repo_file_before_the_others() {
        let error = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n  configPaths: [bad]\n")
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
    fn checks_a_units_name_before_its_targets() {
        let error = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  name: BAD\n  targets: [nope]\n",
            )
            .error();
        assert!(error.contains("`BAD` isn't a valid name"), "{error}");
    }

    #[test]
    fn rejects_a_duplicate_name() {
        let error = base()
            .file("units/a/unit.yaml", "unit:\n  name: app\n")
            .file("units/b/unit.yaml", "unit:\n  name: app\n")
            .error();
        assert_eq!(
            error,
            ".obelisk-k8s/units/b/unit.yaml: unit name `app` is already used by .obelisk-k8s/units/a/unit.yaml"
        );
    }

    #[test]
    fn rejects_a_config_path_without_dot_slash() {
        let error = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  configPaths: [config/x.yaml]\n",
            )
            .error();
        assert!(
            error.starts_with(".obelisk-k8s/units/app/unit.yaml:2:17: config path `config/x.yaml` must start with `./`"),
            "{error}"
        );
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
        assert!(
            error.contains("the directory name `My_App` isn't a valid name"),
            "{error}"
        );
        let error = base()
            .file("units/app/unit.yaml", "unit:\n  name: -app\n")
            .error();
        assert!(error.contains("`-app` isn't a valid name"), "{error}");
        assert!(is_valid_name(&"a".repeat(63)));
        assert!(!is_valid_name(&"a".repeat(64)));
        assert!(!is_valid_name(""));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_dir_needs_a_name() {
        use std::os::unix::ffi::OsStrExt;
        let named = base();
        let dir = named
            .0
            .path()
            .join(".obelisk-k8s/units")
            .join(std::ffi::OsStr::from_bytes(b"app-\xff"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("unit.yaml"), "unit: {}\n").unwrap();
        assert!(named.error().contains("the directory name isn't UTF-8"));
        std::fs::write(dir.join("unit.yaml"), "unit:\n  name: app\n").unwrap();
        assert_eq!(names(&named.load().unwrap().units, |u| &u.name), ["app"]);
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
            .file(
                "units/app/unit.yaml",
                "unit:\n  configPaths: [./config/app.yaml]\n",
            )
            .load()
            .unwrap();
        assert_eq!(
            repo.to_string(),
            "repo: repo-1\nhelm repos: (none)\nconfig paths: (none)\n\
             \ntargets:\n  prod:  (.obelisk-k8s/targets/prod/target.yaml)\n    cluster: homelab-1\n    config paths: (none)\n\
             \nunits:\n  app:  (.obelisk-k8s/units/app/unit.yaml)\n    targets: all: prod\n    config paths:\n      - ./config/app.yaml\n"
        );
    }
}
