//! A repo's own config: `.obelisk-k8s/` at the repo root.
//!
//! ```text
//! .obelisk-k8s/
//!   repo.yaml                 repo: { name, helmRepos, configPaths, generators, templates }
//!   targets/<dir>/target.yaml target: { name, cluster, configPaths, generators, templates, release: { name, namespace }, version, ephemeral }
//!   units/<dir>/unit.yaml     unit: { name, configPaths, generators, templates, targets, release: { name, namespace }, version }
//! ```
//!
//! `generators` and `templates` are lists of [`Render`]s: `{ type: gotmpl | file | url | helm |
//! command, … }`.
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
    pub generators: Vec<Render>,
    pub templates: Vec<Render>,
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
    pub generators: Vec<Render>,
    pub templates: Vec<Render>,
    pub release: Release,
    /// The version its units' Helm releases get (`obelisk.version`), interpolated. Parsed here, agreed
    /// on after generation.
    pub version: Option<InterpString>,
    /// Whether it's a stack that runs many times side by side, each copy named on the command line
    /// (`--ephemeral-name`). Missing or null means no.
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    /// Relative to the repo root, e.g. `.obelisk-k8s/units/my-app/unit.yaml`.
    pub file: PathBuf,
    pub config_paths: Vec<InterpString>,
    pub generators: Vec<Render>,
    pub templates: Vec<Render>,
    pub targets: UnitTargets,
    pub release: Release,
    /// The version its Helm release gets (`obelisk.version`), interpolated. Parsed here, agreed on after
    /// generation.
    pub version: Option<InterpString>,
}

/// Something obelisk-k8s renders: a generator (`generators:`, more values from the merged config
/// values) or a template (`templates:`, k8s resources from the generated values). Written as
/// `{ type, … }`:
///
/// ```yaml
/// - { type: gotmpl, path: ./x.yaml.gotmpl }       # a Go template, rendered by Helm
/// - { type: file, path: ./x.yaml }                 # used as it is
/// - { type: url, url: https://…/x.yaml, sha256: … }  # fetched (and checked, with `sha256`), used as it is
/// - { type: helm, chart: metallb, repo: metallb, version: 0.16.1, values: metallb }  # templates only
/// - { type: command, command: [./render.sh, --flag] }  # run, with the values on stdin
/// ```
///
/// A `path` is like a config path: it starts with `./`, is interpolated, and is skipped if it's
/// missing. A `url` and a `command`'s words are interpolated too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Render {
    Gotmpl {
        path: InterpString,
    },
    File {
        path: InterpString,
    },
    Url {
        url: InterpString,
        sha256: Option<String>,
    },
    Helm(HelmChart),
    /// The program first: `./…` resolves from `.obelisk-k8s/`, an absolute path is used as it is, and
    /// a bare name is looked up on `PATH`.
    Command {
        command: Vec<InterpString>,
    },
}

/// An upstream Helm chart, rendered (`helm template`) as the unit's release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelmChart {
    /// Its name in `repo`, or an `oci://` reference.
    pub chart: String,
    /// The chart repository's URL (`repo:` is one, or a name in `repo.yaml`'s `helmRepos`); `None`
    /// for an OCI chart.
    pub repo: Option<String>,
    pub version: String,
    /// Where its values are in the generated values: a dot-separated key (`metallb`, `charts.x`);
    /// none means no values.
    pub values: Option<String>,
}

impl Render {
    /// Its type, as written.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Gotmpl { .. } => "gotmpl",
            Self::File { .. } => "file",
            Self::Url { .. } => "url",
            Self::Helm(_) => "helm",
            Self::Command { .. } => "command",
        }
    }
}

/// What a target or unit says about the Helm release its units become: `release: { name, namespace }`,
/// both optional and interpolated. Like every value in these files, a plain scalar is read as its text
/// (`name: 123` is `"123"`). Agreed on, with config files and generators, after the generators.
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
        let helm_repos = repo.helm_repos.unwrap_or_default();
        let generators = check_renders(
            repo_file,
            repo.generators,
            RenderList::Generators,
            &helm_repos,
        )?;
        let templates = check_renders(
            repo_file,
            repo.templates,
            RenderList::Templates,
            &helm_repos,
        )?;

        let mut targets = Vec::new();
        for entry in read_entries(&dir, shown, "targets", "target.yaml", &mut warnings)? {
            let TargetFile { target } = yaml::read_file(&entry.path, &entry.file)?;
            targets.push(Target {
                name: name_or_dir(&entry, target.name)?,
                config_paths: check_config_paths(&entry.file, target.config_paths)?,
                generators: check_renders(
                    &entry.file,
                    target.generators,
                    RenderList::Generators,
                    &helm_repos,
                )?,
                templates: check_renders(
                    &entry.file,
                    target.templates,
                    RenderList::Templates,
                    &helm_repos,
                )?,
                cluster: target.cluster,
                release: target.release.unwrap_or_default(),
                version: target.version,
                ephemeral: target.ephemeral.unwrap_or(false),
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
                generators: check_renders(
                    &entry.file,
                    unit.generators,
                    RenderList::Generators,
                    &helm_repos,
                )?,
                templates: check_renders(
                    &entry.file,
                    unit.templates,
                    RenderList::Templates,
                    &helm_repos,
                )?,
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

        warnings.extend(unpinned_urls(&generators, &templates, &targets, &units));
        targets.sort_by(|a, b| a.name.cmp(&b.name));
        units.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            name: repo.name.value,
            helm_repos,
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
            if target.ephemeral {
                writeln!(f, "    ephemeral: yes")?;
            }
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
    generators: Option<Vec<RenderIn>>,
    templates: Option<Vec<RenderIn>>,
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
    generators: Option<Vec<RenderIn>>,
    templates: Option<Vec<RenderIn>>,
    release: Option<Release>,
    version: Option<InterpString>,
    ephemeral: Option<bool>,
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
    generators: Option<Vec<RenderIn>>,
    templates: Option<Vec<RenderIn>>,
    targets: Option<Vec<Spanned<String>>>,
    release: Option<Release>,
    version: Option<InterpString>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderIn {
    #[serde(rename = "type")]
    kind: Spanned<String>,
    path: Option<Spanned<InterpString>>,
    url: Option<InterpString>,
    sha256: Option<String>,
    chart: Option<String>,
    repo: Option<String>,
    version: Option<String>,
    values: Option<String>,
    command: Option<Vec<InterpString>>,
}

/// Which list renders are in: generators make values, templates make resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderList {
    Generators,
    Templates,
}

impl RenderList {
    fn what(self) -> &'static str {
        match self {
            Self::Generators => GENERATOR_PATH,
            Self::Templates => TEMPLATE_PATH,
        }
    }
}

fn check_renders(
    file: &Path,
    renders: Option<Vec<RenderIn>>,
    list: RenderList,
    helm_repos: &IndexMap<String, String>,
) -> Result<Vec<Render>, ObeliskK8sError> {
    renders
        .unwrap_or_default()
        .into_iter()
        .map(|r| check_render(file, r, list, helm_repos))
        .collect()
}

fn check_render(
    file: &Path,
    r: RenderIn,
    list: RenderList,
    helm_repos: &IndexMap<String, String>,
) -> Result<Render, ObeliskK8sError> {
    let at = Some(r.kind.at);
    let error = |message: String| config_error(file, at, message);
    let kind = r.kind.value.as_str();
    let given = [
        ("path", r.path.is_some()),
        ("url", r.url.is_some()),
        ("sha256", r.sha256.is_some()),
        ("chart", r.chart.is_some()),
        ("repo", r.repo.is_some()),
        ("version", r.version.is_some()),
        ("values", r.values.is_some()),
        ("command", r.command.is_some()),
    ];
    let takes: &[&str] = match kind {
        "gotmpl" | "file" => &["path"],
        "url" => &["url", "sha256"],
        "helm" => &["chart", "repo", "version", "values"],
        "command" => &["command"],
        _ => {
            return Err(error(format!(
                "unknown type `{kind}` (expected gotmpl, file, url, helm or command)"
            )));
        }
    };
    if let Some((extra, _)) = given.iter().find(|(key, set)| *set && !takes.contains(key)) {
        return Err(error(format!(
            "`type: {kind}` takes {}, not `{extra}`",
            takes
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let required = |key: &str, set: bool| {
        if set {
            Ok(())
        } else {
            Err(error(format!("`type: {kind}` needs `{key}`")))
        }
    };
    match kind {
        "gotmpl" | "file" => {
            let path = r
                .path
                .ok_or_else(|| error(format!("`type: {kind}` needs `path`")))?;
            let path = check_paths(file, Some(vec![path]), list.what())?.remove(0);
            Ok(if kind == "gotmpl" {
                Render::Gotmpl { path }
            } else {
                Render::File { path }
            })
        }
        "url" => {
            let url = r
                .url
                .ok_or_else(|| error("`type: url` needs `url`".to_owned()))?;
            check_url(url, r.sha256).map_err(error)
        }
        "helm" => {
            if list == RenderList::Generators {
                return Err(error(
                    "a Helm chart renders resources, not values: put it in `templates:`".to_owned(),
                ));
            }
            required("chart", r.chart.is_some())?;
            required("version", r.version.is_some())?;
            let chart = HelmChart {
                chart: r.chart.unwrap_or_default(),
                repo: r.repo,
                version: r.version.unwrap_or_default(),
                values: r.values,
            };
            check_helm(chart, helm_repos).map_err(error)
        }
        _ => {
            let command = r.command.unwrap_or_default();
            let Some(program) = command.first().map(InterpString::raw) else {
                return Err(error(
                    "`type: command` needs a `command`: the program, then its arguments".to_owned(),
                ));
            };
            // `./x`, `/abs/x` or a bare name: not a path relative to who knows where.
            if program.is_empty()
                || (program.contains('/')
                    && !program.starts_with("./")
                    && !program.starts_with('/'))
            {
                return Err(error(format!(
                    "program `{program}` must be `./…` (from `{REPO_CONFIG_DIR}/`), an absolute path, or a name on `PATH`"
                )));
            }
            Ok(Render::Command { command })
        }
    }
}

/// A warning for each `url` render without a `sha256`, by the file declaring it.
fn unpinned_urls(
    generators: &[Render],
    templates: &[Render],
    targets: &[Target],
    units: &[Unit],
) -> Vec<String> {
    let declared = std::iter::once((Path::new(REPO_FILE), generators, templates))
        .chain(targets.iter().map(|t| {
            (
                t.file.as_path(),
                t.generators.as_slice(),
                t.templates.as_slice(),
            )
        }))
        .chain(units.iter().map(|u| {
            (
                u.file.as_path(),
                u.generators.as_slice(),
                u.templates.as_slice(),
            )
        }));
    let mut warnings = Vec::new();
    for (file, generators, templates) in declared {
        for render in generators.iter().chain(templates) {
            if let Render::Url { url, sha256: None } = render {
                warnings.push(format!(
                    "{}: url `{url}` has no `sha256`: what it fetches can change under you",
                    file.display()
                ));
            }
        }
    }
    warnings
}

/// A `type: url`: https, and a `sha256` (if any) that looks like one.
fn check_url(url: InterpString, sha256: Option<String>) -> Result<Render, String> {
    if !url.raw().starts_with("https://") {
        return Err(format!(
            "url `{url}` must be https:// (it's applied to clusters)"
        ));
    }
    if let Some(sha) = &sha256
        && (sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(format!(
            "sha256 `{sha}` isn't a SHA-256: 64 lowercase hex digits"
        ));
    }
    Ok(Render::Url { url, sha256 })
}

/// A `type: helm`'s chart, with `repo` (as written) resolved: an `oci://` chart takes none; any
/// other takes a URL, or a name in `helm_repos`.
fn check_helm(
    mut chart: HelmChart,
    helm_repos: &IndexMap<String, String>,
) -> Result<Render, String> {
    let name = &chart.chart;
    chart.repo = match (name.starts_with("oci://"), chart.repo.take()) {
        (true, None) => None,
        (true, Some(_)) => {
            return Err(format!(
                "chart `{name}` is an OCI reference: it takes no `repo`"
            ));
        }
        (false, None) => {
            return Err(format!(
                "chart `{name}` needs a `repo` (a URL, or a name in `helmRepos`), or to be an `oci://` reference"
            ));
        }
        (false, Some(repo)) if repo.starts_with("https://") || repo.starts_with("http://") => {
            Some(repo)
        }
        (false, Some(repo)) => Some(helm_repos.get(&repo).cloned().ok_or_else(|| {
            let known: Vec<_> = helm_repos.keys().map(String::as_str).collect();
            format!(
                "repo `{repo}` isn't a URL or a name in `helmRepos` (there are: {})",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            )
        })?),
    };
    Ok(Render::Helm(chart))
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
        let Render::Gotmpl { path } = &repo.generators[0] else {
            panic!("{:?}", repo.generators[0])
        };
        assert_eq!(path.raw(), "./g.gotmpl");
        assert_eq!(repo.units[0].generators, []);
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  templates: [{ type: gotmpl, path: ./t.gotmpl }]\n",
            )
            .load()
            .unwrap();
        assert_eq!(repo.templates[0].kind(), "gotmpl");
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
        let e = error("unit:\n  generators: [{ type: nope, path: ./g }]\n");
        assert!(e.contains("unknown type `nope`"), "{e}");
        let e = error("unit:\n  generators: [{ type: gotmpl, path: ./g, extra: 1 }]\n");
        assert!(e.contains("extra"), "{e}");
        let e = error("unit:\n  generators: [{ path: ./g }]\n");
        assert!(e.contains("type"), "{e}");
    }

    #[test]
    fn loads_every_render_type() {
        let sha = "a".repeat(64);
        let repo = base()
            .file(
                "repo.yaml",
                "repo:\n  name: r\n  helmRepos: { metallb: https://metallb.github.io/metallb }\n",
            )
            .file(
                "units/app/unit.yaml",
                &format!(
                    "unit:\n  generators:\n    - {{ type: file, path: ./v.yaml }}\n    - {{ type: url, url: https://x/v.yaml }}\n    - {{ type: command, command: [./gen.sh, \"${{{{obelisk:target}}}}\"] }}\n  templates:\n    - {{ type: url, url: https://x/crds.yaml, sha256: {sha} }}\n    - {{ type: helm, chart: metallb, repo: metallb, version: 0.16.1, values: metallb }}\n    - {{ type: helm, chart: oci://docker.io/envoyproxy/gateway-helm, version: 1.9.3 }}\n    - {{ type: helm, chart: x, repo: https://charts.example.com, version: \"1\" }}\n"
                ),
            )
            .load()
            .unwrap();
        let unit = &repo.units[0];
        let kinds: Vec<_> = unit.generators.iter().map(Render::kind).collect();
        assert_eq!(kinds, ["file", "url", "command"]);
        let Render::Command { command } = &unit.generators[2] else {
            panic!("{:?}", unit.generators[2])
        };
        assert_eq!(command[1].raw(), "${{obelisk:target}}");
        assert_eq!(
            unit.templates[1],
            Render::Helm(HelmChart {
                chart: "metallb".to_owned(),
                repo: Some("https://metallb.github.io/metallb".to_owned()),
                version: "0.16.1".to_owned(),
                values: Some("metallb".to_owned()),
            })
        );
        let Render::Helm(oci) = &unit.templates[2] else {
            panic!()
        };
        assert_eq!(oci.repo, None);
        let Render::Helm(by_url) = &unit.templates[3] else {
            panic!()
        };
        assert_eq!(by_url.repo.as_deref(), Some("https://charts.example.com"));
    }

    #[test]
    fn rejects_bad_renders() {
        let error = |list: &str, render: &str| {
            base()
                .file(
                    "units/app/unit.yaml",
                    &format!("unit:\n  {list}: [{render}]\n"),
                )
                .error()
        };
        for (list, render, expected) in [
            (
                "generators",
                "{ type: helm, chart: x, repo: https://x, version: 1 }",
                "a Helm chart renders resources, not values",
            ),
            (
                "templates",
                "{ type: file, url: https://x }",
                "`type: file` takes `path`, not `url`",
            ),
            ("templates", "{ type: file }", "`type: file` needs `path`"),
            (
                "templates",
                "{ type: url, url: ftp://x }",
                "must be https://",
            ),
            (
                "templates",
                "{ type: url, url: https://x, sha256: ABC }",
                "isn't a SHA-256",
            ),
            (
                "templates",
                "{ type: helm, chart: x, version: 1 }",
                "needs a `repo`",
            ),
            (
                "templates",
                "{ type: helm, chart: x, repo: nope, version: 1 }",
                "repo `nope` isn't a URL or a name in `helmRepos` (there are: none)",
            ),
            (
                "templates",
                "{ type: helm, chart: oci://x/y, repo: https://x, version: 1 }",
                "takes no `repo`",
            ),
            (
                "templates",
                "{ type: helm, chart: x, repo: https://x }",
                "`type: helm` needs `version`",
            ),
            (
                "templates",
                "{ type: command, command: [] }",
                "needs a `command`",
            ),
            (
                "templates",
                "{ type: command, command: [bin/gen] }",
                "must be `./…`",
            ),
        ] {
            let e = error(list, render);
            assert!(e.contains(expected), "{render}: {e}");
        }
    }

    #[test]
    fn warns_about_an_unpinned_url() {
        let repo = base()
            .file(
                "units/app/unit.yaml",
                "unit:\n  templates: [{ type: url, url: https://x/y.yaml }]\n",
            )
            .load()
            .unwrap();
        assert_eq!(
            repo.warnings,
            [
                ".obelisk-k8s/units/app/unit.yaml: url `https://x/y.yaml` has no `sha256`: what it fetches can change under you"
            ]
        );
    }

    #[test]
    fn ephemeral_defaults_to_false() {
        let repo = Repo::new()
            .file("repo.yaml", "repo:\n  name: r\n")
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
            Repo::new()
                .file("repo.yaml", "repo:\n  name: r\n")
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
