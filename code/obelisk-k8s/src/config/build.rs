//! A unit's build file, and the build plan it renders to for a case (`template build`).
//!
//! The build file (`build:` in `unit.yaml`, else `repo.yaml`; see `config::paths`) is a Go template,
//! rendered by Helm through a throwaway chart with just the `obelisk:` block as `.Values`. `.Release`
//! is `obelisk-build` / `default` whatever the unit, as for a Go template values source (the real
//! release isn't agreed on yet: use `.Values.obelisk`). It must output one mapping:
//!
//! ```yaml
//! build:
//!   phases:
//!     - name: base                                    # a valid name, unique in the plan
//!       values:                                       # values sources
//!         - { type: inline, values: { replicas: 1 } }
//!         - { type: gotmpl, path: ./values.yaml.gotmpl }
//!     - name: out
//!       manifests:                                    # manifests sources
//!         - { type: gotmpl, path: ./app.yaml.gotmpl }
//!       images: []                                    # reserved: only empty, for now
//! ```
//!
//! A phase may have values sources and manifests sources (either, both, or neither), in any order and
//! number. Everything in a phase renders against the values of the phases before it (see
//! `config::phases`). A source is `{ type, … }`:
//!
//! - `{ type: inline, values }`: a values mapping, as it is (values only).
//! - `{ type: gotmpl, path }`: a Go template, rendered by Helm through a throwaway chart.
//! - `{ type: file, path }`: used as it is.
//! - `{ type: url, url, sha256? }`: fetched (https only), checked against `sha256` when given.
//! - `{ type: helm, chart, repo?, version, values? }`: an upstream chart (manifests only); `repo` is a
//!   URL or a name in `repo.yaml`'s `helmRepos`, and none for an `oci://` chart.
//! - `{ type: command, command: [program, args…] }`: run with the values on stdin. The program is
//!   `./…` (from `.obelisk-k8s/`), an absolute path, or a name on `PATH`.
//!
//! A `path` is absolute, or starts with `./` (from `.obelisk-k8s/`); an absolute one under the repo
//! root (e.g. from `.Values.obelisk.paths.repo.obelisk`) is shown relative to it, like a `./…` one,
//! so messages and `# Source:` comments are the same on every machine. A `gotmpl` or `file` source
//! whose file is missing is skipped. Nothing in the plan is `${{obelisk:…}}`-interpolated: it's a Go
//! template's output, so it already has `.Values.obelisk`. The plan's strings are read like a values
//! file's: a plain `1` is a number, so quote a `version: "1"`.
//!
//! A plan that's wrong says where (`build.phases[0].values[1]: …`). Its warnings (a `url` without a
//! `sha256`) are handed on as soon as it's built (`Warnings`), before anything renders from it.

use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::de::value::MapAccessDeserializer;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::ephemeral::Ephemeral;
use crate::config::global::{GlobalConfig, normalize};
use crate::config::paths::{Selection, from_repo_config, write_yaml_cases};
use crate::config::repo::{REPO_CONFIG_DIR, RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{shown_under, values_yaml};
use crate::config::{NAME_RULE, is_valid_name};
use crate::{helm, yaml};

/// What `.Release.Name` is while a build file or Go template values source renders (the real one
/// isn't agreed on yet). Fixed, so it's a valid Helm release name whatever the unit is called.
pub(crate) const RENDER_RELEASE: &str = "obelisk-build";

/// What `.Release.Namespace` is while a build file or Go template values source renders.
pub(crate) const RENDER_NAMESPACE: &str = "default";

/// A case's build plan: its build file's output, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildPlan {
    /// The build file's output (`build: { phases: […] }`), as it rendered.
    pub output: Value,
    pub phases: Vec<Phase>,
    /// Things that look wrong but don't stop it (a `url` without a `sha256`).
    pub warnings: Vec<String>,
}

/// One phase: its values sources and manifests sources, in the order written.
#[derive(Debug, Clone, PartialEq)]
pub struct Phase {
    pub name: String,
    pub values: Vec<Source>,
    pub manifests: Vec<Source>,
}

/// Something a phase renders: values (a mapping) or manifests (k8s resources).
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Values as written in the plan. `at` labels it, for messages: the build file (relative to the
    /// repo root, or absolute), then its place in the plan (`build.phases[0].values[1]`).
    Inline {
        values: Value,
        at: PathBuf,
    },
    /// Relative to the repo root (`./…` was from `.obelisk-k8s/`), or absolute; and whether it
    /// exists (if not, it's skipped).
    Gotmpl {
        path: PathBuf,
        found: bool,
    },
    /// As [`Source::Gotmpl`].
    File {
        path: PathBuf,
        found: bool,
    },
    Url {
        url: String,
        sha256: Option<String>,
    },
    Helm(HelmChart),
    /// The program (`./…` made relative to the repo root), then its arguments.
    Command(Vec<String>),
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
    /// Where its values are in the values: a dot-separated key (`metallb`, `charts.x`); none means
    /// no values.
    pub values: Option<String>,
}

impl Source {
    /// Its type, as written.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Inline { .. } => "inline",
            Self::Gotmpl { .. } => "gotmpl",
            Self::File { .. } => "file",
            Self::Url { .. } => "url",
            Self::Helm(_) => "helm",
            Self::Command(_) => "command",
        }
    }

    /// Whether it renders: everything but a missing file does.
    #[must_use]
    pub fn present(&self) -> bool {
        !matches!(
            self,
            Self::Gotmpl { found: false, .. } | Self::File { found: false, .. }
        )
    }

    /// What it is, for messages, `# Source:` comments and what a values source set under
    /// `obelisk:`: its path (relative to the repo root), URL, chart, where it's written (inline), or
    /// (not its arguments, which may hold secrets) a command's program.
    pub(crate) fn describe(&self) -> PathBuf {
        match self {
            Self::Inline { at: path, .. } | Self::Gotmpl { path, .. } | Self::File { path, .. } => {
                path.clone()
            }
            Self::Url { url, .. } => url.into(),
            Self::Helm(chart) => format!("{}@{}", chart.chart, chart.version).into(),
            Self::Command(words) => words[0].clone().into(),
        }
    }

    /// [`Source::describe`], as the user should see it: a path (an inline source's build file
    /// included) under `shown_root`.
    pub(crate) fn shown(&self, shown_root: Option<&Path>) -> PathBuf {
        match self {
            Self::Inline { at: path, .. } | Self::Gotmpl { path, .. } | Self::File { path, .. } => {
                shown_under(shown_root, path)
            }
            _ => self.describe(),
        }
    }
}

impl Phase {
    /// Whether any of its manifests sources renders.
    #[must_use]
    pub fn has_manifests(&self) -> bool {
        self.manifests.iter().any(Source::present)
    }
}

impl BuildPlan {
    /// Whether any phase has a manifests source that renders.
    #[must_use]
    pub fn has_manifests(&self) -> bool {
        self.phases.iter().any(Phase::has_manifests)
    }
}

/// One (target, unit) case's build plan, as YAML.
#[derive(Debug)]
pub struct BuildCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    pub plan: BuildPlan,
    /// The plan's output, as YAML.
    pub yaml: String,
}

/// What `template build` prints: a single case's plan as plain YAML, or several as a YAML stream, each
/// document under a magenta `# [ target=…, unit=… ]` comment.
#[derive(Debug)]
pub struct BuildReport<'a> {
    pub cases: Vec<BuildCase<'a>>,
    pub single: bool,
}

impl RepoConfig {
    /// The build plan of every case in `selection`. `shown_root` is as for
    /// [`RepoConfig::values_report`]; `warn` hears each plan's warnings as it's built, once each.
    ///
    /// # Errors
    ///
    /// As [`RunInputs::new`]; [`ObeliskK8sError::Helm`] if a build file doesn't render;
    /// [`ObeliskK8sError::Config`] if one is missing, or its output isn't a plan.
    pub fn build_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        warn: impl FnMut(&str),
    ) -> Result<BuildReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, None)?;
        let mut cases = Vec::new();
        let mut warnings = Warnings::new(warn);
        for (target, unit) in &selection.cases {
            let obelisk = self.obelisk_values(target, unit, &inputs)?;
            let plan = self.build_plan(repo_root, shown_root, target, unit, &obelisk)?;
            warnings.add(&plan.warnings);
            let name = format!(
                "the build plan for unit `{}` on target `{}`",
                unit.name, target.name
            );
            let yaml = yaml::to_string(&plan.output, Path::new(&name))?;
            cases.push(BuildCase {
                target: &target.name,
                unit: &unit.name,
                plan,
                yaml,
            });
        }
        Ok(BuildReport {
            cases,
            single: selection.single,
        })
    }

    /// `unit`'s build plan on `target`: its build file, rendered (with `obelisk`, the case's
    /// [`RepoConfig::obelisk_values`], as `.Values`) and checked.
    pub(crate) fn build_plan(
        &self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &Target,
        unit: &Unit,
        obelisk: &Map<String, Value>,
    ) -> Result<BuildPlan, ObeliskK8sError> {
        let build = self.build_path(repo_root, target, unit)?;
        let shown = shown_under(shown_root, &build.path);
        if !build.found {
            return Err(ObeliskK8sError::Config {
                file: shown_under(shown_root, build.declared_in),
                at: None,
                message: format!(
                    "the build file of unit `{}` on target `{}`, {}, doesn't exist",
                    unit.name,
                    target.name,
                    shown.display()
                ),
            });
        }
        let text = helm::render(
            &repo_root.join(&build.path),
            &shown,
            &values_yaml(obelisk, target, unit)?,
            RENDER_RELEASE,
            RENDER_NAMESPACE,
            None,
        )?;
        parse_plan(&text, &build.path, &shown, &self.helm_repos, repo_root)
    }
}

/// Hands build plans' warnings on, each the first time it's seen: as soon as its plan is built, so
/// it's out before anything renders from the plan (or installs), and isn't lost if that fails.
pub(crate) struct Warnings<F> {
    seen: Vec<String>,
    warn: F,
}

impl<F: FnMut(&str)> Warnings<F> {
    pub(crate) fn new(warn: F) -> Self {
        Self {
            seen: Vec::new(),
            warn,
        }
    }

    /// Hands on the ones not seen yet.
    pub(crate) fn add(&mut self, warnings: &[String]) {
        for warning in warnings {
            if !self.seen.contains(warning) {
                (self.warn)(warning);
                self.seen.push(warning.clone());
            }
        }
    }
}

impl fmt::Display for BuildReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cases = self
            .cases
            .iter()
            .map(|c| (c.target, c.unit, c.yaml.as_str()));
        write_yaml_cases(f, self.single, "---\n", cases)
    }
}

/// Which list a source is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Values,
    Manifests,
}

impl List {
    /// Its key in a phase.
    fn key(self) -> &'static str {
        match self {
            Self::Values => "values",
            Self::Manifests => "manifests",
        }
    }
}

// A build file's output, as written: decoded first (so a shape error says where, e.g.
// `build.phases[0].values[1]`), then checked into a `BuildPlan` by `Checker`. Decoded from the
// values tree, not the text, so a plain `1` stays a number (and isn't a string).

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuildFile {
    build: Mapping<RawBuild>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    phases: Option<Vec<Mapping<RawPhase>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhase {
    name: String,
    values: Option<Vec<Mapping<RawSource>>>,
    manifests: Option<Vec<Mapping<RawSource>>>,
    /// Reserved: only empty, for now.
    images: Option<Vec<IgnoredAny>>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawSource {
    Inline {
        values: Option<Map<String, Value>>,
    },
    Gotmpl {
        path: String,
    },
    File {
        path: String,
    },
    Url {
        url: String,
        sha256: Option<String>,
    },
    Helm {
        chart: String,
        repo: Option<String>,
        version: String,
        values: Option<String>,
    },
    Command {
        command: Vec<String>,
    },
}

/// A `T` that must be written as a mapping: serde would also take a list of its fields, in order.
struct Mapping<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Mapping<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OnlyMap<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for OnlyMap<T> {
            type Value = T;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map))
            }
        }
        deserializer
            .deserialize_map(OnlyMap(PhantomData))
            .map(Mapping)
    }
}

/// Parses a build file's output (`text`) into its plan. `file` is the build file (relative to the
/// repo root, or absolute; inline sources are labelled with it), `shown` the same as the user should
/// see it (in errors and warnings), and `repo_root` where `./…` paths are looked for.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it isn't YAML; [`ObeliskK8sError::Config`] if it isn't one
/// `build: { phases }` mapping, or a phase or source in it is wrong.
pub(crate) fn parse_plan(
    text: &str,
    file: &Path,
    shown: &Path,
    helm_repos: &IndexMap<String, String>,
    repo_root: &Path,
) -> Result<BuildPlan, ObeliskK8sError> {
    let error = |message: String| ObeliskK8sError::Config {
        file: shown.to_owned(),
        at: None,
        message,
    };
    let mut documents = yaml::parse_values_stream(text, shown)?;
    let output = match (documents.pop(), documents.len()) {
        (Some(output), 0) => output,
        (output, more) => {
            let count = more + usize::from(output.is_some());
            return Err(error(format!(
                "a build file must output one mapping, `build: {{ phases: […] }}`, not {count} documents"
            )));
        }
    };
    let Mapping(raw): Mapping<RawBuildFile> =
        serde_path_to_error::deserialize(&output).map_err(|e| error(decode_message(e)))?;
    let mut checker = Checker {
        file,
        shown,
        helm_repos,
        repo_root,
        absolute_root: std::path::absolute(repo_root).ok().map(|r| normalize(&r)),
        warnings: Vec::new(),
    };
    let phases = checker.plan(raw).map_err(error)?;
    Ok(BuildPlan {
        output,
        phases,
        warnings: checker.warnings,
    })
}

/// A decoding error, at its key path (`build.phases[0].values[1]`).
fn decode_message(error: serde_path_to_error::Error<serde_json::Error>) -> String {
    let at = error.path().to_string();
    let message = error.into_inner().to_string();
    // The plan is read like a values file: a plain `1` is a number.
    let hint = if message.ends_with("expected a string") {
        " (quote it)"
    } else {
        ""
    };
    if at == "." {
        format!("the output: {message}{hint}")
    } else {
        format!("`{at}`: {message}{hint}")
    }
}

/// Checking a decoded plan: what goes wrong is at a key path (`build.phases[0].values[1]`).
struct Checker<'a> {
    file: &'a Path,
    shown: &'a Path,
    helm_repos: &'a IndexMap<String, String>,
    repo_root: &'a Path,
    /// `repo_root`, absolute and normalized (as `obelisk.paths.repo.root` is), if it can be.
    absolute_root: Option<PathBuf>,
    warnings: Vec<String>,
}

impl Checker<'_> {
    fn plan(&mut self, raw: RawBuildFile) -> Result<Vec<Phase>, String> {
        let mut phases: Vec<Phase> = Vec::new();
        for (i, Mapping(phase)) in raw
            .build
            .0
            .phases
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            let at = format!("build.phases[{i}]");
            let phase = self.phase(phase, &at)?;
            if phases.iter().any(|p| p.name == phase.name) {
                return Err(format!(
                    "`{at}.name`: phase `{}` is already in the plan (names are unique)",
                    phase.name
                ));
            }
            phases.push(phase);
        }
        Ok(phases)
    }

    fn phase(&mut self, raw: RawPhase, at: &str) -> Result<Phase, String> {
        let name = raw.name;
        if !is_valid_name(&name) {
            return Err(format!(
                "`{at}.name`: `{name}` isn't a valid name ({NAME_RULE})"
            ));
        }
        if raw.images.is_some_and(|images| !images.is_empty()) {
            return Err(format!(
                "`{at}.images`: images aren't supported yet (leave it out, or empty)"
            ));
        }
        Ok(Phase {
            name,
            values: self.sources(raw.values, at, List::Values)?,
            manifests: self.sources(raw.manifests, at, List::Manifests)?,
        })
    }

    fn sources(
        &mut self,
        raw: Option<Vec<Mapping<RawSource>>>,
        at: &str,
        list: List,
    ) -> Result<Vec<Source>, String> {
        raw.unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, Mapping(source))| {
                let at = format!("{at}.{}[{i}]", list.key());
                self.source(source, &at, list)
                    .map_err(|message| format!("`{at}`: {message}"))
            })
            .collect()
    }

    fn source(&mut self, raw: RawSource, at: &str, list: List) -> Result<Source, String> {
        match (raw, list) {
            (RawSource::Inline { .. }, List::Manifests) => {
                Err("`type: inline` makes values, not manifests: put it in `values:`".to_owned())
            }
            (RawSource::Helm { .. }, List::Values) => {
                Err("a Helm chart renders resources, not values: put it in `manifests:`".to_owned())
            }
            (RawSource::Inline { values }, List::Values) => Ok(Source::Inline {
                values: values.map_or(Value::Null, Value::Object),
                at: format!("{} ({at})", self.file.display()).into(),
            }),
            (RawSource::Gotmpl { path }, _) => {
                let (path, found) = self.path(&path)?;
                Ok(Source::Gotmpl { path, found })
            }
            (RawSource::File { path }, _) => {
                let (path, found) = self.path(&path)?;
                Ok(Source::File { path, found })
            }
            (RawSource::Url { url, sha256 }, _) => {
                let source = check_url(url, sha256)?;
                if let Source::Url { url, sha256: None } = &source {
                    self.warnings.push(format!(
                        "{}: url `{url}` has no `sha256`: what it fetches can change under you",
                        self.shown.display()
                    ));
                }
                Ok(source)
            }
            (
                RawSource::Helm {
                    chart,
                    repo,
                    version,
                    values,
                },
                List::Manifests,
            ) => check_helm(
                HelmChart {
                    chart,
                    repo,
                    version,
                    values,
                },
                self.helm_repos,
            ),
            (RawSource::Command { command: words }, _) => command(words),
        }
    }

    /// A `gotmpl` or `file` source's `path` (as written: absolute, or `./…` from `.obelisk-k8s/`),
    /// relative to the repo root if it's under it; and whether it exists.
    fn path(&self, written: &str) -> Result<(PathBuf, bool), String> {
        if !written.starts_with("./") && !written.starts_with('/') {
            return Err(format!(
                "path `{written}` must be absolute, or start with `./` (from `{REPO_CONFIG_DIR}/`)"
            ));
        }
        let path = self.in_repo(from_repo_config(written));
        let found = self.repo_root.join(&path).is_file();
        Ok((path, found))
    }

    /// `path`, relative to the repo root if it's an absolute path under it (e.g. from
    /// `.Values.obelisk.paths.repo.obelisk`): so it's shown the same on every machine.
    fn in_repo(&self, path: PathBuf) -> PathBuf {
        match &self.absolute_root {
            Some(root) if path.is_absolute() => normalize(&path)
                .strip_prefix(root)
                .map_or(path, Path::to_path_buf),
            _ => path,
        }
    }
}

/// A `type: command`'s words: the program first, `./…` (from `.obelisk-k8s/`), absolute, or a name
/// on `PATH`.
fn command(mut words: Vec<String>) -> Result<Source, String> {
    let Some(program) = words.first() else {
        return Err(
            "`type: command` needs a `command`: the program, then its arguments".to_owned(),
        );
    };
    // `./x`, `/abs/x` or a bare name: not a path relative to who knows where.
    if program.is_empty()
        || (program.contains('/') && !program.starts_with("./") && !program.starts_with('/'))
    {
        return Err(format!(
            "program `{program}` must be `./…` (from `{REPO_CONFIG_DIR}/`), an absolute path, or a name on `PATH`"
        ));
    }
    if program.starts_with("./") {
        words[0] = from_repo_config(program).display().to_string();
    }
    Ok(Source::Command(words))
}

/// A `type: url`: https, and a `sha256` (if any) that looks like one.
fn check_url(url: String, sha256: Option<String>) -> Result<Source, String> {
    if !url.starts_with("https://") {
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
    Ok(Source::Url { url, sha256 })
}

/// A `type: helm`'s chart, with `repo` (as written) resolved: an `oci://` chart takes none; any
/// other takes a URL, or a name in `helm_repos`.
fn check_helm(
    mut chart: HelmChart,
    helm_repos: &IndexMap<String, String>,
) -> Result<Source, String> {
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
    Ok(Source::Helm(chart))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};

    const SHOWN: &str = ".obelisk-k8s/build.yaml.gotmpl";

    fn parse(text: &str) -> Result<BuildPlan, ObeliskK8sError> {
        let dir = write_repo(&[("here.yaml", "")]);
        let repos: IndexMap<String, String> = [(
            "metallb".to_owned(),
            "https://metallb.github.io/metallb".to_owned(),
        )]
        .into();
        parse_plan(text, Path::new(SHOWN), Path::new(SHOWN), &repos, dir.path())
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    /// One phase `p` with `key: [source]`.
    fn source_error(key: &str, source: &str) -> String {
        error(&format!(
            "build:\n  phases:\n    - name: p\n      {key}: [{source}]\n"
        ))
    }

    #[test]
    fn parses_phases_and_every_source_type() {
        let sha = "a".repeat(64);
        let plan = parse(&format!(
            "build:\n  phases:\n    - name: base\n      values:\n        - {{ type: inline, values: {{ a: 1 }} }}\n        - {{ type: gotmpl, path: ./here.yaml }}\n        - {{ type: file, path: /nowhere/x.yaml }}\n        - {{ type: url, url: https://x/v.yaml, sha256: {sha} }}\n        - {{ type: command, command: [./gen.sh, prod] }}\n    - name: out\n      manifests:\n        - {{ type: helm, chart: metallb, repo: metallb, version: 0.16.1, values: metallb }}\n        - {{ type: helm, chart: oci://docker.io/envoyproxy/gateway-helm, version: 1.9.3 }}\n        - {{ type: command, command: [sh, -c, x] }}\n      images: []\n    - name: empty\n"
        ))
        .unwrap();
        let names: Vec<_> = plan.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["base", "out", "empty"]);
        let kinds: Vec<_> = plan.phases[0].values.iter().map(Source::kind).collect();
        assert_eq!(kinds, ["inline", "gotmpl", "file", "url", "command"]);
        assert_eq!(
            plan.phases[0].values[0],
            Source::Inline {
                values: serde_json::json!({ "a": 1 }),
                at: format!("{SHOWN} (build.phases[0].values[0])").into(),
            }
        );
        // `./…` is from `.obelisk-k8s/`; a missing file is kept, and skipped when it renders.
        assert_eq!(
            plan.phases[0].values[1],
            Source::Gotmpl {
                path: ".obelisk-k8s/here.yaml".into(),
                found: true
            }
        );
        assert_eq!(
            plan.phases[0].values[2],
            Source::File {
                path: "/nowhere/x.yaml".into(),
                found: false
            }
        );
        assert!(!plan.phases[0].values[2].present());
        assert_eq!(
            plan.phases[0].values[4],
            Source::Command(vec![".obelisk-k8s/gen.sh".into(), "prod".into()])
        );
        assert_eq!(
            plan.phases[1].manifests[0],
            Source::Helm(HelmChart {
                chart: "metallb".to_owned(),
                repo: Some("https://metallb.github.io/metallb".to_owned()),
                version: "0.16.1".to_owned(),
                values: Some("metallb".to_owned()),
            })
        );
        assert!(plan.has_manifests());
        assert!(!plan.phases[0].has_manifests());
        assert_eq!(plan.phases[2].values, []);
        assert_eq!(plan.warnings, Vec::<String>::new());
    }

    #[test]
    fn an_absolute_path_in_the_repo_is_shown_from_it() {
        let dir = write_repo(&[("here.yaml", "")]);
        let obelisk = dir.path().join(".obelisk-k8s");
        let text = format!(
            "build:\n  phases:\n    - name: p\n      values:\n        - {{ type: file, path: {}/here.yaml }}\n        - {{ type: file, path: {}/../.obelisk-k8s/nope.yaml }}\n",
            obelisk.display(),
            obelisk.display()
        );
        let shown = Path::new(SHOWN);
        let plan = parse_plan(&text, shown, shown, &IndexMap::new(), dir.path()).unwrap();
        assert_eq!(
            plan.phases[0].values,
            [
                Source::File {
                    path: ".obelisk-k8s/here.yaml".into(),
                    found: true
                },
                Source::File {
                    path: ".obelisk-k8s/nope.yaml".into(),
                    found: false
                },
            ]
        );
    }

    #[test]
    fn missing_and_null_are_empty() {
        for text in [
            "build:\n  phases: null\n",
            "build: {}\n",
            "build:\n  phases: []\n",
        ] {
            assert_eq!(parse(text).unwrap().phases, [], "{text:?}");
        }
        let plan = parse("build:\n  phases:\n    - { name: p, values: null, manifests: null, images: null }\n    - { name: q, values: [{ type: inline }] }\n").unwrap();
        assert_eq!(plan.phases[0].values, []);
        assert!(matches!(
            &plan.phases[1].values[0],
            Source::Inline {
                values: Value::Null,
                ..
            }
        ));
    }

    #[test]
    fn the_output_must_be_one_build_mapping() {
        let one =
            format!("{SHOWN}: a build file must output one mapping, `build: {{ phases: […] }}`");
        assert_eq!(error(""), format!("{one}, not 0 documents"));
        assert_eq!(
            error("build: {}\n---\nbuild: {}\n"),
            format!("{one}, not 2 documents")
        );
        for (text, expected) in [
            (
                "- a\n",
                "the output: invalid type: sequence, expected a mapping",
            ),
            ("{}\n", "the output: missing field `build`"),
            (
                "build: {}\nphases: []\n",
                "`phases`: unknown field `phases`, expected `build`",
            ),
            (
                "build: null\n",
                "`build`: invalid type: null, expected a mapping",
            ),
            (
                "build: { phases: {} }\n",
                "`build.phases`: invalid type: map, expected a sequence",
            ),
        ] {
            assert_eq!(error(text), format!("{SHOWN}: {expected}"), "{text:?}");
        }
    }

    #[test]
    fn checks_phases() {
        let phases = |text: &str| error(&format!("build:\n  phases:\n{text}"));
        assert_eq!(
            phases("    - { values: [] }\n"),
            format!("{SHOWN}: `build.phases[0]`: missing field `name`")
        );
        // A list of its fields (which serde would take) isn't a phase.
        assert_eq!(
            phases("    - [a]\n"),
            format!("{SHOWN}: `build.phases[0]`: invalid type: sequence, expected a mapping")
        );
        assert_eq!(
            phases("    - { name: Bad_Name }\n"),
            format!("{SHOWN}: `build.phases[0].name`: `Bad_Name` isn't a valid name ({NAME_RULE})")
        );
        assert_eq!(
            phases("    - { name: a }\n    - { name: a }\n"),
            format!(
                "{SHOWN}: `build.phases[1].name`: phase `a` is already in the plan (names are unique)"
            )
        );
        assert_eq!(
            phases("    - { name: a, nmae: b }\n"),
            format!(
                "{SHOWN}: `build.phases[0].nmae`: unknown field `nmae`, expected one of `name`, `values`, `manifests`, `images`"
            )
        );
        assert_eq!(
            phases("    - { name: a, images: [app] }\n"),
            format!(
                "{SHOWN}: `build.phases[0].images`: images aren't supported yet (leave it out, or empty)"
            )
        );
        assert_eq!(
            phases("    - { name: 1 }\n"),
            format!(
                "{SHOWN}: `build.phases[0].name`: invalid type: integer `1`, expected a string (quote it)"
            )
        );
    }

    #[test]
    fn rejects_bad_sources() {
        for (key, source, expected) in [
            (
                "values",
                "{ path: ./x }",
                "`build.phases[0].values[0]`: missing field `type`",
            ),
            (
                "values",
                "{ type: nope }",
                "`build.phases[0].values[0].type`: unknown variant `nope`, expected one of `inline`, `gotmpl`, `file`, `url`, `helm`, `command`",
            ),
            (
                "values",
                "{ type: helm, chart: x, repo: https://x, version: \"1\" }",
                "`build.phases[0].values[0]`: a Helm chart renders resources, not values: put it in `manifests:`",
            ),
            (
                "manifests",
                "{ type: inline, values: {} }",
                "`build.phases[0].manifests[0]`: `type: inline` makes values, not manifests: put it in `values:`",
            ),
            (
                "values",
                "{ type: inline, values: [a] }",
                "`build.phases[0].values[0]`: invalid type: sequence, expected a map",
            ),
            (
                "manifests",
                "{ type: file, url: https://x }",
                "`build.phases[0].manifests[0]`: unknown field `url`, expected `path`",
            ),
            (
                "manifests",
                "{ type: file }",
                "`build.phases[0].manifests[0]`: missing field `path`",
            ),
            (
                "values",
                "{ type: gotmpl, path: x.gotmpl }",
                "`build.phases[0].values[0]`: path `x.gotmpl` must be absolute, or start with `./` (from `.obelisk-k8s/`)",
            ),
            (
                "manifests",
                "{ type: url, url: ftp://x }",
                "must be https://",
            ),
            (
                "manifests",
                "{ type: url, url: https://x, sha256: ABC }",
                "isn't a SHA-256",
            ),
            (
                "manifests",
                "{ type: helm, chart: x, version: \"1\" }",
                "needs a `repo`",
            ),
            (
                "manifests",
                "{ type: helm, chart: x, repo: nope, version: \"1\" }",
                "repo `nope` isn't a URL or a name in `helmRepos` (there are: metallb)",
            ),
            (
                "manifests",
                "{ type: helm, chart: oci://x/y, repo: https://x, version: \"1\" }",
                "takes no `repo`",
            ),
            (
                "manifests",
                "{ type: helm, chart: x, repo: https://x }",
                "missing field `version`",
            ),
            (
                "manifests",
                "{ type: helm, chart: x, repo: https://x, version: 1 }",
                "`build.phases[0].manifests[0]`: invalid type: integer `1`, expected a string (quote it)",
            ),
            (
                "values",
                "{ type: command, command: [] }",
                "needs a `command`",
            ),
            (
                "values",
                "{ type: command, command: [bin/gen] }",
                "must be `./…`",
            ),
            (
                "values",
                "{ type: command, command: [gen, 1] }",
                "`build.phases[0].values[0]`: invalid type: integer `1`, expected a string (quote it)",
            ),
        ] {
            let e = source_error(key, source);
            assert!(e.starts_with(&format!("{SHOWN}: ")), "{source}: {e}");
            assert!(e.contains(expected), "{source}: {e}");
        }
    }

    #[test]
    fn warns_about_an_unpinned_url() {
        let plan = parse(
            "build:\n  phases:\n    - name: p\n      manifests: [{ type: url, url: https://x/y.yaml }]\n",
        )
        .unwrap();
        assert_eq!(
            plan.warnings,
            [format!(
                "{SHOWN}: url `https://x/y.yaml` has no `sha256`: what it fetches can change under you"
            )]
        );
    }

    fn report(dir: &tempfile::TempDir, targets: &[&str]) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        let selection = config.select(&targets, &[]).unwrap();
        let text = config
            .build_report(dir.path(), &selection, None, None, &global(), |_| {})?
            .to_string();
        Ok(anstream::adapter::strip_str(&text).to_string())
    }

    #[test]
    fn renders_the_build_file_with_the_obelisk_block() {
        if !helm_installed() {
            return;
        }
        let dir = write_repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  build: \"${{obelisk:path:unit}}/build.yaml.gotmpl\"\n",
            ),
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
            ("units/app/unit.yaml", "unit: {}\n"),
            (
                "units/app/build.yaml.gotmpl",
                "{{- $o := .Values.obelisk }}\nbuild:\n  phases:\n    - name: {{ $o.target }}\n      values:\n        - type: file\n          path: {{ $o.paths.repo.obelisk }}/{{ $o.target }}.yaml\n        - type: inline\n          values:\n            release: {{ .Release.Name }}/{{ .Release.Namespace }}\n            keys: {{ keys .Values | sortAlpha | join \",\" }}\n",
            ),
        ]);
        let text = report(&dir, &["dev", "prod"]).unwrap();
        let obelisk = dir.path().join(".obelisk-k8s");
        let obelisk = obelisk.display();
        assert_eq!(
            text,
            format!(
                "# [ target=dev, unit=app ]\nbuild:\n  phases:\n  - name: dev\n    values:\n    - type: file\n      path: {obelisk}/dev.yaml\n    - type: inline\n      values:\n        release: obelisk-build/default\n        keys: obelisk\n---\n\
                 # [ target=prod, unit=app ]\nbuild:\n  phases:\n  - name: prod\n    values:\n    - type: file\n      path: {obelisk}/prod.yaml\n    - type: inline\n      values:\n        release: obelisk-build/default\n        keys: obelisk\n"
            )
        );
    }

    #[test]
    fn a_missing_build_file_is_an_error() {
        let dir = write_repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./nope.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            ("units/app/unit.yaml", "unit: {}\n"),
        ]);
        assert_eq!(
            report(&dir, &["prod"]).unwrap_err().to_string(),
            ".obelisk-k8s/repo.yaml: the build file of unit `app` on target `prod`, .obelisk-k8s/nope.gotmpl, doesn't exist"
        );
    }

    #[test]
    fn a_unit_longer_than_a_helm_release_name_still_renders() {
        if !helm_installed() {
            return;
        }
        // The build file renders as a fixed release, not one named after the unit.
        let long = "a".repeat(63);
        let dir = write_repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./b.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            (&format!("units/{long}/unit.yaml"), "unit: {}\n"),
            (
                "b.gotmpl",
                "build:\n  phases: [{ name: {{ .Release.Name }} }]\n",
            ),
        ]);
        assert_eq!(
            report(&dir, &["prod"]).unwrap(),
            "build:\n  phases:\n  - name: obelisk-build\n"
        );
    }

    #[test]
    fn warnings_are_handed_on_as_each_plan_is_built_once_each() {
        if !helm_installed() {
            return;
        }
        let dir = write_repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./b.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
            ("targets/qa/target.yaml", TARGET),
            ("units/app/unit.yaml", "unit: {}\n"),
            (
                "b.gotmpl",
                "{{- if eq .Values.obelisk.target \"prod\" }}{{ fail \"boom\" }}{{ end }}\nbuild:\n  phases: [{ name: p, manifests: [{ type: url, url: https://x/y.yaml }] }]\n",
            ),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["dev".to_owned(), "qa".to_owned(), "prod".to_owned()], &[])
            .unwrap();
        let mut warnings = Vec::new();
        // `dev` and `qa` warn the same (once), then `prod`'s build file fails: the warning's out.
        let result = config.build_report(dir.path(), &selection, None, None, &global(), |w| {
            warnings.push(w.to_owned());
        });
        assert!(result.is_err());
        assert_eq!(
            warnings,
            [
                ".obelisk-k8s/b.gotmpl: url `https://x/y.yaml` has no `sha256`: what it fetches can change under you"
            ]
        );
    }

    #[test]
    fn a_bad_build_file_names_itself() {
        if !helm_installed() {
            return;
        }
        let dir = write_repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./b.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            ("units/app/unit.yaml", "unit: {}\n"),
            ("b.gotmpl", "build: {{ .Values.nope.deeper }}\n"),
        ]);
        let e = report(&dir, &["prod"]).unwrap_err().to_string();
        assert!(
            e.starts_with(".obelisk-k8s/b.gotmpl: helm couldn't render it: "),
            "{e}"
        );
        std::fs::write(dir.path().join(".obelisk-k8s/b.gotmpl"), "phases: []\n").unwrap();
        let e = report(&dir, &["prod"]).unwrap_err().to_string();
        assert_eq!(
            e,
            ".obelisk-k8s/b.gotmpl: `phases`: unknown field `phases`, expected `build`"
        );
    }
}
