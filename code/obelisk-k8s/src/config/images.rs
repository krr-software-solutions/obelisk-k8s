//! The images a unit declares on a target (`template images`): its build plan's images sources,
//! each rendered against the values of the phases before its own (see `config::phases`), as image
//! specs, checked; and building them (`images build`: see `docker`, and `config::push` for pushing).
//!
//! An images source renders as a manifests source does (see `config::render` for the types), but
//! its output is image specs: a YAML stream, one spec per document (`---` between them), or none.
//! An `inline` one lists them (`{ type: inline, images: [ … ] }`). A spec is:
//!
//! ```yaml
//! name: my-image                                    # a valid name, unique in the case
//! version: "1.2.3"                                  # a docker tag (no `+`): the template decides
//! preBuild:                                         # optional: run before the image builds
//!   type: file                                      # a script (or `type: command`, `command: [ … ]`)
//!   path: ./units/my-app/pre-image-build.sh
//!   cwd: ./units/my-app                             # optional: where it runs (default: the repo root)
//! dockerfile:
//!   type: file
//!   path: ./units/my-app/Dockerfile
//! contextPath: ./..                                 # optional: the build context (default: the repo root)
//! ```
//!
//! Paths are written as in the build plan (absolute, or `./…` from `.obelisk-k8s/`), and kept and
//! printed absolute and normalized; only messages show them relative to the repo root. The Dockerfile, a `preBuild` script, its `cwd` and the `contextPath` must exist. A
//! built image's ref isn't added to the values: templates make it from the same `name` and
//! `version`. A unit with no images sources declares none, and its values phases don't run.
//!
//! `images build` builds each tag (`<name>:<version>`) once, however many cases declare it: its
//! `preBuild`, then `docker build`. One tag declared two ways is an error. Without `--no-push`, it
//! then pushes them (`config::push`).

use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::build::{BuildPlan, RepoPaths, Source, Warnings};
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::paths::{Selection, write_yaml_cases};
use crate::config::render;
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{values_yaml, with_release};
use crate::config::{NAME_RULE, is_valid_name};
use crate::{ObeliskK8sError, yaml};

/// An image spec, checked: what an image is called and tagged, and how it builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_build: Option<PreBuild>,
    pub dockerfile: Dockerfile,
    /// The build context: what the Dockerfile's `COPY`s see. Absolute and normalized.
    pub context_path: PathBuf,
}

impl Image {
    /// Its tag: `<name>:<version>`.
    #[must_use]
    pub fn tag(&self) -> String {
        format!("{}:{}", self.name, self.version)
    }
}

/// What runs before an image builds, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreBuild {
    #[serde(flatten)]
    pub run: Run,
    /// Absolute and normalized.
    pub cwd: PathBuf,
}

/// A script file to run, or a program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Run {
    /// Absolute and normalized.
    File { path: PathBuf },
    /// The program (`./…` made absolute), then its arguments.
    Command { command: Vec<String> },
}

/// Where an image's Dockerfile is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Dockerfile {
    /// Absolute and normalized.
    File { path: PathBuf },
}

/// An image, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredImage {
    pub image: Image,
    /// The phase whose images sources declared it.
    pub phase: String,
    /// The source, as the user should see it.
    pub source: PathBuf,
}

/// One (target, unit) case's images.
#[derive(Debug)]
pub struct ImagesCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// In phase order, then source order, then the order each source gave them.
    pub images: Vec<DeclaredImage>,
}

/// What `template images` prints: a single case's image specs as a YAML stream, or several, each
/// under a magenta `# [ target=…, unit=… ]` comment.
#[derive(Debug)]
pub struct ImagesReport<'a> {
    pub cases: Vec<ImagesCase<'a>>,
    pub single: bool,
    /// Each case's images, as YAML.
    yaml: Vec<String>,
}

impl RepoConfig {
    /// The images of every case in `selection`. `shown_root` and `warn` are as for
    /// [`RepoConfig::values_report`].
    ///
    /// # Errors
    ///
    /// Whatever running the values phases fails with; [`ObeliskK8sError::Helm`] if an images source
    /// doesn't render; [`ObeliskK8sError::Config`] if its output isn't image specs, or a name is
    /// declared twice in a case.
    pub fn images_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        warn: impl FnMut(&str),
    ) -> Result<ImagesReport<'a>, ObeliskK8sError> {
        // Nothing in an image spec is resolved: the values' SSM refs are only checked.
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, None)?;
        let mut cases = Vec::new();
        let mut yaml = Vec::new();
        let mut warnings = Warnings::new(warn);
        for (target, unit) in &selection.cases {
            let obelisk = self.obelisk_values(target, unit, &inputs)?;
            let plan = self.build_plan(repo_root, shown_root, target, unit, &obelisk)?;
            warnings.add(&plan.warnings);
            let case =
                Self::images_for(repo_root, shown_root, target, unit, &inputs, &plan, obelisk)?;
            yaml.push(images_yaml(&case)?);
            cases.push(case);
        }
        Ok(ImagesReport {
            cases,
            single: selection.single,
            yaml,
        })
    }

    /// One case's images, from `plan`. `obelisk` is as for [`RepoConfig::run_values`].
    pub(crate) fn images_for<'a>(
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
        plan: &BuildPlan,
        obelisk: Map<String, Value>,
    ) -> Result<ImagesCase<'a>, ObeliskK8sError> {
        let mut images: Vec<DeclaredImage> = Vec::new();
        // Nothing to render: skip the values phases too (each source may be a `helm` run).
        if plan.has_images() {
            let evaluated =
                Self::run_values(repo_root, shown_root, target, unit, inputs, plan, obelisk)?;
            let agreed = evaluated.agreed;
            let paths = RepoPaths::new(repo_root);
            // Phase k renders against the values before it, V₍ₖ₋₁₎: kept for each phase with images.
            for (phase, before) in plan.phases.iter().zip(evaluated.before) {
                // Kept for a phase with manifests too: only ones with images render here.
                let Some(before) = before.filter(|_| phase.has_images()) else {
                    continue;
                };
                let values = with_release(before, &agreed);
                let values_yaml = values_yaml(&values, target, unit)?;
                let render_inputs = render::Inputs {
                    repo_root,
                    values: &values,
                    values_yaml: &values_yaml,
                    release: &agreed.release_name,
                    namespace: &agreed.namespace,
                    chart_version: None,
                    capabilities: None,
                };
                for source in phase.images.iter().filter(|s| s.present()) {
                    let shown = source.shown(repo_root, shown_root);
                    for image in source_images(source, &shown, &render_inputs, &paths)? {
                        if let Some(first) = images.iter().find(|i| i.image.name == image.name) {
                            return Err(ObeliskK8sError::Config {
                                file: shown,
                                at: None,
                                message: format!(
                                    "image `{}` is already declared, by {} (names are unique in a unit)",
                                    image.name,
                                    first.source.display()
                                ),
                            });
                        }
                        images.push(DeclaredImage {
                            image,
                            phase: phase.name.clone(),
                            source: shown.clone(),
                        });
                    }
                }
            }
        }
        Ok(ImagesCase {
            target: &target.name,
            unit: &unit.name,
            images,
        })
    }
}

/// The image specs one images source (shown as `shown`) outputs.
fn source_images(
    source: &Source,
    shown: &Path,
    render_inputs: &render::Inputs<'_>,
    paths: &RepoPaths,
) -> Result<Vec<Image>, ObeliskK8sError> {
    // An inline source in `images:` lists its specs (see `config::build`); it's never rendered.
    let documents = if let Source::Inline { values, .. } = source {
        match values {
            Value::Array(images) => images.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    } else {
        let text = render::text(source, shown, render_inputs)?;
        yaml::parse_values_stream(&text, shown)?
    };
    documents
        .into_iter()
        // Numbered by place in the stream (or list), empty documents included.
        .enumerate()
        // A template that renders nothing (or only a comment) gives an empty document.
        .filter(|(_, document)| !document.is_null())
        .map(|(i, document)| {
            parse_image(&document, paths).map_err(|message| ObeliskK8sError::Config {
                file: shown.to_owned(),
                at: None,
                message: format!("image spec {}: {message}", i + 1),
            })
        })
        .collect()
}

/// What `images build` built: each image (by tag, in the order first declared), and the cases
/// that declare it.
#[derive(Debug)]
pub struct BuiltImages<'a> {
    pub images: Vec<Built<'a>>,
}

/// `(target, unit)` cases, e.g. those that declare an image.
pub type Cases<'a> = Vec<(&'a str, &'a str)>;

/// One image `images build` built.
#[derive(Debug)]
pub struct Built<'a> {
    pub tag: String,
    pub cases: Cases<'a>,
}

impl RepoConfig {
    /// Builds the images of every case in `selection` (only those named `only`, if any): each
    /// distinct image once, its `preBuild` then `docker build`. `shown_root` and `warn` are as for
    /// [`RepoConfig::values_report`]; `step` hears each step before it starts.
    ///
    /// # Errors
    ///
    /// As [`RepoConfig::images_report`]; [`ObeliskK8sError::Usage`] if `only` names an image no
    /// case declares, or one tag is declared differently by two cases; [`ObeliskK8sError::Image`]
    /// if a `preBuild` or `docker build` fails (nothing after it builds).
    #[allow(clippy::too_many_arguments)]
    pub fn images_build<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        only: &[String],
        warn: impl FnMut(&str),
        mut step: impl FnMut(&str),
    ) -> Result<BuiltImages<'a>, ObeliskK8sError> {
        let report =
            self.images_report(repo_root, selection, shown_root, ephemeral, global, warn)?;
        let mut images = Vec::new();
        for (tag, image, cases) in distinct(&report, only)? {
            build_one(&tag, image, repo_root, &mut step)?;
            images.push(Built { tag, cases });
        }
        Ok(BuiltImages { images })
    }
}

/// Builds `image` (tagged `tag`): its `preBuild`, then `docker build`, telling `step` each.
pub(crate) fn build_one(
    tag: &str,
    image: &Image,
    repo_root: &Path,
    step: &mut impl FnMut(&str),
) -> Result<(), ObeliskK8sError> {
    if image.pre_build.is_some() {
        step(&format!("Running the `preBuild` of image `{tag}`"));
        crate::docker::pre_build(image, repo_root)?;
    }
    step(&format!("Building image `{tag}`"));
    crate::docker::build(image)
}

/// `report`'s images, each tag once (with the image and the cases that declare it), only those
/// named `only` if any.
pub(crate) fn distinct<'r, 'a>(
    report: &'r ImagesReport<'a>,
    only: &[String],
) -> Result<Vec<(String, &'r Image, Cases<'a>)>, ObeliskK8sError> {
    let mut builds: Vec<(String, &Image, Cases<'a>)> = Vec::new();
    for case in &report.cases {
        for declared in &case.images {
            let image = &declared.image;
            if !only.is_empty() && !only.contains(&image.name) {
                continue;
            }
            let tag = image.tag();
            match builds.iter_mut().find(|(t, _, _)| *t == tag) {
                Some((_, first, cases)) if *first == image => cases.push((case.target, case.unit)),
                Some((_, _, cases)) => {
                    let (target, unit) = cases[0];
                    return Err(ObeliskK8sError::Usage(format!(
                        "image `{tag}` is declared differently by unit `{}` on target `{}` and unit `{unit}` on target `{target}`: one tag must be one image",
                        case.unit, case.target
                    )));
                }
                None => builds.push((tag, image, vec![(case.target, case.unit)])),
            }
        }
    }
    if let Some(missing) = only.iter().find(|name| {
        !report
            .cases
            .iter()
            .any(|c| c.images.iter().any(|i| &i.image.name == *name))
    }) {
        let mut known: Vec<&str> = report
            .cases
            .iter()
            .flat_map(|c| c.images.iter().map(|i| i.image.name.as_str()))
            .collect();
        known.sort_unstable();
        known.dedup();
        return Err(ObeliskK8sError::Usage(format!(
            "no selected unit declares image `{missing}` (they declare: {})",
            if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            }
        )));
    }
    Ok(builds)
}

impl fmt::Display for BuiltImages<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.images.is_empty() {
            return writeln!(f, "No images to build");
        }
        for built in &self.images {
            let cases: Vec<String> = built
                .cases
                .iter()
                .map(|(target, unit)| format!("{unit} on {target}"))
                .collect();
            writeln!(f, "Built {} ({})", built.tag, cases.join(", "))?;
        }
        Ok(())
    }
}

// An image spec, as written: decoded first (so a shape error says where, e.g. `dockerfile.path`),
// then checked into an `Image`.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawImage {
    name: String,
    version: String,
    pre_build: Option<RawPreBuild>,
    dockerfile: RawDockerfile,
    context_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawPreBuild {
    File {
        path: String,
        cwd: Option<String>,
    },
    Command {
        command: Vec<String>,
        cwd: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawDockerfile {
    File { path: String },
}

/// One image spec (`document`), checked.
fn parse_image(document: &Value, paths: &RepoPaths) -> Result<Image, String> {
    if !document.is_object() {
        return Err("must be a mapping (`name`, `version`, `dockerfile`, …)".to_owned());
    }
    let raw: RawImage = serde_path_to_error::deserialize(document).map_err(|e| {
        let at = e.path().to_string();
        let message = e.into_inner().to_string();
        let hint = if message.ends_with("expected a string") {
            " (quote it)"
        } else {
            ""
        };
        if at == "." {
            format!("{message}{hint}")
        } else {
            format!("`{at}`: {message}{hint}")
        }
    })?;
    if !is_valid_name(&raw.name) {
        return Err(format!(
            "`name`: `{}` isn't a valid name ({NAME_RULE})",
            raw.name
        ));
    }
    if raw.version.is_empty() {
        return Err("`version`: is empty".to_owned());
    }
    if !is_valid_tag(&raw.version) {
        return Err(format!(
            "`version`: `{}` isn't a valid image tag ({TAG_RULE})",
            raw.version
        ));
    }
    let pre_build = raw.pre_build.map(|raw| pre_build(raw, paths)).transpose()?;
    let RawDockerfile::File { path } = raw.dockerfile;
    let dockerfile = Dockerfile::File {
        path: existing(paths, paths.file(&path), "dockerfile.path", "file")?,
    };
    let context_path = match raw.context_path {
        Some(context) => existing(paths, paths.dir(&context), "contextPath", "directory")?,
        None => paths.root().to_owned(),
    };
    Ok(Image {
        name: raw.name,
        version: raw.version,
        pre_build,
        dockerfile,
        context_path,
    })
}

const TAG_RULE: &str =
    "at most 128 letters, digits, `_`, `.` and `-`, not starting with `.` or `-`";

/// A docker tag: `version` is one (`<name>:<version>`).
fn is_valid_tag(version: &str) -> bool {
    let bytes = version.as_bytes();
    let word = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_';
    bytes.len() <= 128
        && bytes.first().is_some_and(word)
        && bytes.iter().all(|b| word(b) || *b == b'.' || *b == b'-')
}

fn pre_build(raw: RawPreBuild, paths: &RepoPaths) -> Result<PreBuild, String> {
    let (run, cwd) = match raw {
        RawPreBuild::File { path, cwd } => (
            Run::File {
                path: existing(paths, paths.file(&path), "preBuild.path", "file")?,
            },
            cwd,
        ),
        RawPreBuild::Command {
            command: words,
            cwd,
        } => (
            Run::Command {
                command: paths
                    .command(words)
                    .map_err(|message| format!("`preBuild`: {message}"))?,
            },
            cwd,
        ),
    };
    let cwd = match cwd {
        Some(cwd) => existing(paths, paths.dir(&cwd), "preBuild.cwd", "directory")?,
        None => paths.root().to_owned(),
    };
    Ok(PreBuild { run, cwd })
}

/// A checked path (at key `at`) that must exist, as a `what`.
fn existing(
    paths: &RepoPaths,
    checked: Result<(PathBuf, bool), String>,
    at: &str,
    what: &str,
) -> Result<PathBuf, String> {
    match checked {
        Ok((path, true)) => Ok(path),
        Ok((path, false)) => Err(format!(
            "`{at}`: {} doesn't exist (or isn't a {what})",
            paths.shown(&path).display()
        )),
        Err(message) => Err(format!("`{at}`: {message}")),
    }
}

/// A case's images as a YAML stream: each spec a document under a `# Source:` comment naming its
/// source and phase.
fn images_yaml(case: &ImagesCase<'_>) -> Result<String, ObeliskK8sError> {
    let mut text = String::new();
    for declared in &case.images {
        let name = format!(
            "image `{}` of unit `{}` on target `{}`",
            declared.image.name, case.unit, case.target
        );
        let spec = yaml::to_string(&declared.image, Path::new(&name))?;
        let _ = write!(
            text,
            "---\n# Source: {} (phase `{}`)\n{spec}",
            declared.source.display(),
            declared.phase
        );
        if !spec.ends_with('\n') {
            text.push('\n');
        }
    }
    Ok(text)
}

impl fmt::Display for ImagesReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Every image starts with `---`, so cases need nothing between them.
        let cases = self
            .cases
            .iter()
            .zip(&self.yaml)
            .map(|(c, yaml)| (c.target, c.unit, yaml.as_str()));
        write_yaml_cases(f, self.single, "", cases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::build::parse_plan;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};

    /// Target `prod`, unit `app` (version 1.0.0), a Dockerfile and a pre-build script; `files` on
    /// top.
    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let mut all = vec![
            ("repo.yaml", "repo:\n  name: r\n  build: ./build.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.0.0 }\n",
            ),
            ("app/Dockerfile", "FROM scratch\n"),
            ("app/pre.sh", "true\n"),
        ];
        all.extend_from_slice(files);
        write_repo(&all)
    }

    /// Unit `app` on `prod`, its build file having output `output`: its images.
    fn images(
        dir: &tempfile::TempDir,
        output: &str,
    ) -> Result<Vec<DeclaredImage>, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["prod".to_owned()], &["app".to_owned()])
            .unwrap();
        let global = global();
        let inputs = RunInputs::new(dir.path(), &selection, None, &global, None)?;
        let (target, unit) = selection.cases[0];
        let shown = Path::new(".obelisk-k8s/build.gotmpl");
        let plan = parse_plan(output, shown, shown, &config.helm_repos, dir.path())?;
        let obelisk = config.obelisk_values(target, unit, &inputs)?;
        let case = RepoConfig::images_for(dir.path(), None, target, unit, &inputs, &plan, obelisk)?;
        Ok(case.images)
    }

    /// One phase `p` with `images: [source]`.
    fn one(source: &str) -> String {
        format!("build:\n  phases:\n    - name: p\n      images: [{source}]\n")
    }

    /// One phase `p` with an inline images source listing `specs`.
    fn inline(specs: &str) -> String {
        one(&format!("{{ type: inline, images: [{specs}] }}"))
    }

    const DOCKERFILE: &str = "dockerfile: { type: file, path: ./app/Dockerfile }";

    fn error(output: &str) -> String {
        images(&repo(&[]), output).unwrap_err().to_string()
    }

    #[test]
    fn parses_a_full_spec() {
        let dir = repo(&[]);
        let found = images(
            &dir,
            &inline(&format!(
                "{{ name: app, version: \"1\", preBuild: {{ type: file, path: ./app/pre.sh, cwd: ./app }}, {DOCKERFILE} }}, {{ name: tool, version: v2, preBuild: {{ type: command, command: [./app/pre.sh, x] }}, {DOCKERFILE}, contextPath: ./app }}"
            )),
        )
        .unwrap();
        // Paths are absolute.
        let root = crate::config::build::absolute_root(dir.path());
        let dockerfile = Dockerfile::File {
            path: root.join(".obelisk-k8s/app/Dockerfile"),
        };
        assert_eq!(
            found.iter().map(|i| i.image.clone()).collect::<Vec<_>>(),
            [
                Image {
                    name: "app".to_owned(),
                    version: "1".to_owned(),
                    pre_build: Some(PreBuild {
                        run: Run::File {
                            path: root.join(".obelisk-k8s/app/pre.sh")
                        },
                        cwd: root.join(".obelisk-k8s/app"),
                    }),
                    dockerfile: dockerfile.clone(),
                    context_path: root.clone(),
                },
                Image {
                    name: "tool".to_owned(),
                    version: "v2".to_owned(),
                    pre_build: Some(PreBuild {
                        run: Run::Command {
                            command: vec![
                                root.join(".obelisk-k8s/app/pre.sh").display().to_string(),
                                "x".to_owned()
                            ]
                        },
                        cwd: root.clone(),
                    }),
                    dockerfile,
                    context_path: root.join(".obelisk-k8s/app"),
                },
            ]
        );
        assert_eq!(found[0].phase, "p");
    }

    #[test]
    fn a_source_may_output_several_specs_or_none() {
        let dir = repo(&[]);
        let output = one(&format!(
            "{{ type: command, command: [printf, \"---\\\\nname: a\\\\nversion: x\\\\n{DOCKERFILE}\\\\n---\\\\n# nothing\\\\n---\\\\nname: b\\\\nversion: v2\\\\n{DOCKERFILE}\\\\n\"] }}"
        ));
        let names: Vec<_> = images(&dir, &output)
            .unwrap()
            .into_iter()
            .map(|i| i.image.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn images_see_the_values_before_their_phase_not_their_own() {
        let dir = repo(&[]);
        // The version is how many lines of its values start with `a:`.
        let source = format!(
            "{{ type: command, command: [sh, -c, \"printf 'name: %s\\\\nversion: \\\"%s\\\"\\\\n{DOCKERFILE}\\\\n' $0 $(grep -c '^a:' || true)\", NAME] }}"
        );
        let output = format!(
            "build:\n  phases:\n    - name: one\n      values: [{{ type: inline, values: {{ a: 1 }} }}]\n      images: [{}]\n    - name: two\n      images: [{}]\n",
            source.replace("NAME", "one"),
            source.replace("NAME", "two")
        );
        let found: Vec<_> = images(&dir, &output)
            .unwrap()
            .into_iter()
            .map(|i| (i.phase, i.image.version))
            .collect();
        assert_eq!(
            found,
            [
                ("one".to_owned(), "0".to_owned()),
                ("two".to_owned(), "1".to_owned())
            ]
        );
    }

    #[test]
    fn a_unit_without_images_declares_none() {
        // Its values phases don't run: no release is needed.
        let dir = repo(&[("units/app/unit.yaml", "unit: {}\n")]);
        let output = "build:\n  phases:\n    - name: p\n      values: [{ type: inline, values: { a: 1 } }]\n      images: [{ type: gotmpl, path: ./missing.gotmpl }]\n";
        assert_eq!(images(&dir, output).unwrap(), []);
    }

    #[test]
    fn rejects_bad_specs() {
        let source = ".obelisk-k8s/build.gotmpl (build.phases[0].images[0])";
        for (specs, expected) in [
            ("app".to_owned(), "image spec 1: must be a mapping"),
            (
                format!("{{ name: a, {DOCKERFILE} }}"),
                "image spec 1: missing field `version`",
            ),
            (
                format!("{{ name: a, version: 1, {DOCKERFILE} }}"),
                "image spec 1: `version`: invalid type: integer `1`, expected a string (quote it)",
            ),
            (
                format!("{{ name: a, version: \"\", {DOCKERFILE} }}"),
                "image spec 1: `version`: is empty",
            ),
            (
                format!("{{ name: a, version: 1.0.0+abc, {DOCKERFILE} }}"),
                "image spec 1: `version`: `1.0.0+abc` isn't a valid image tag",
            ),
            (
                format!("{{ name: Bad_Name, version: x, {DOCKERFILE} }}"),
                "image spec 1: `name`: `Bad_Name` isn't a valid name",
            ),
            (
                format!("{{ name: a, version: x, tag: y, {DOCKERFILE} }}"),
                "image spec 1: `tag`: unknown field `tag`",
            ),
            (
                "{ name: a, version: x, dockerfile: { type: file, path: ./nope } }".to_owned(),
                "image spec 1: `dockerfile.path`: .obelisk-k8s/nope doesn't exist (or isn't a file)",
            ),
            (
                "{ name: a, version: x, dockerfile: { type: file, path: app/Dockerfile } }"
                    .to_owned(),
                "image spec 1: `dockerfile.path`: path `app/Dockerfile` must be absolute, or start with `./`",
            ),
            (
                format!(
                    "{{ name: a, version: x, preBuild: {{ type: file, path: ./app/pre.sh, cwd: ./nope }}, {DOCKERFILE} }}"
                ),
                "image spec 1: `preBuild.cwd`: .obelisk-k8s/nope doesn't exist (or isn't a directory)",
            ),
            (
                format!("{{ name: a, version: x, {DOCKERFILE}, contextPath: ./nope }}"),
                "image spec 1: `contextPath`: .obelisk-k8s/nope doesn't exist (or isn't a directory)",
            ),
            (
                format!(
                    "{{ name: a, version: x, preBuild: {{ type: command, command: [] }}, {DOCKERFILE} }}"
                ),
                "image spec 1: `preBuild`: `type: command` needs a `command`",
            ),
            (
                format!("~, {{ name: a, version: 1, {DOCKERFILE} }}"),
                "image spec 2: `version`: invalid type",
            ),
            (
                format!(
                    "{{ name: a, version: x, {DOCKERFILE} }}, {{ name: b, version: x, dockerfile: {{ type: url }} }}"
                ),
                "image spec 2: `dockerfile.type`: unknown variant `url`, expected `file`",
            ),
        ] {
            let e = error(&inline(&specs));
            assert!(e.starts_with(&format!("{source}: ")), "{specs}: {e}");
            assert!(e.contains(expected), "{specs}: {e}");
        }
    }

    #[test]
    fn paths_are_absolute_and_normalized() {
        let dir = repo(&[]);
        let root = crate::config::build::absolute_root(dir.path());
        let found = images(
            &dir,
            &inline(&format!(
                "{{ name: a, version: x, preBuild: {{ type: file, path: ./app/../app/pre.sh, cwd: ./.. }}, dockerfile: {{ type: file, path: {}/app/Dockerfile }} }}",
                root.join(".obelisk-k8s/../.obelisk-k8s").display()
            )),
        )
        .unwrap();
        let image = found[0].image.clone();
        assert_eq!(
            (image.pre_build.unwrap(), image.dockerfile),
            (
                PreBuild {
                    run: Run::File {
                        path: root.join(".obelisk-k8s/app/pre.sh")
                    },
                    // `./..` is the repo root.
                    cwd: root.clone(),
                },
                Dockerfile::File {
                    path: root.join(".obelisk-k8s/app/Dockerfile")
                },
            )
        );
    }

    #[test]
    fn names_are_unique_in_a_unit() {
        let spec = format!("{{ name: a, version: x, {DOCKERFILE} }}");
        let output = format!(
            "build:\n  phases:\n    - name: p\n      images: [{{ type: inline, images: [{spec}] }}]\n    - name: q\n      images: [{{ type: inline, images: [{spec}] }}]\n"
        );
        assert_eq!(
            error(&output),
            ".obelisk-k8s/build.gotmpl (build.phases[1].images[0]): image `a` is already declared, by .obelisk-k8s/build.gotmpl (build.phases[0].images[0]) (names are unique in a unit)"
        );
    }

    #[test]
    fn go_templates_render_with_the_values_and_release() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[(
            "images.yaml.gotmpl",
            "{{- range list \"a\" \"b\" }}\n---\nname: {{ . }}\nversion: {{ $.Values.obelisk.release.version | quote }}\ndockerfile: { type: file, path: {{ $.Values.obelisk.paths.repo.obelisk }}/app/Dockerfile }\n{{- end }}\n",
        )]);
        let found: Vec<_> = images(&dir, &one("{ type: gotmpl, path: ./images.yaml.gotmpl }"))
            .unwrap()
            .into_iter()
            .map(|i| (i.image.name, i.image.version, i.source))
            .collect();
        let source = PathBuf::from(".obelisk-k8s/images.yaml.gotmpl");
        assert_eq!(
            found,
            [
                ("a".to_owned(), "1.0.0".to_owned(), source.clone()),
                ("b".to_owned(), "1.0.0".to_owned(), source),
            ]
        );
    }

    #[test]
    fn prints_each_spec_under_its_source() {
        let dir = repo(&[]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["prod".to_owned()], &["app".to_owned()])
            .unwrap();
        std::fs::write(
            dir.path().join(".obelisk-k8s/build.gotmpl"),
            inline(&format!("{{ name: a, version: x, {DOCKERFILE} }}")),
        )
        .unwrap();
        if !helm_installed() {
            return;
        }
        let report = config
            .images_report(dir.path(), &selection, None, None, &global(), |_| {})
            .unwrap();
        assert_eq!(
            report.to_string(),
            format!(
                "---\n# Source: .obelisk-k8s/build.gotmpl (build.phases[0].images[0]) (phase `p`)\nname: a\nversion: x\ndockerfile:\n  type: file\n  path: {root}/.obelisk-k8s/app/Dockerfile\ncontextPath: {root}\n",
                root = crate::config::build::absolute_root(dir.path()).display()
            )
        );
    }

    /// `(target, unit, [(name, version)])`.
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// A report of `cases`, each image's Dockerfile `df`.
    fn report<'a>(cases: &[Case<'a>], df: &str) -> ImagesReport<'a> {
        let cases = cases
            .iter()
            .map(|(target, unit, images)| ImagesCase {
                target,
                unit,
                images: images
                    .iter()
                    .map(|(name, version)| DeclaredImage {
                        image: Image {
                            name: (*name).to_owned(),
                            version: (*version).to_owned(),
                            pre_build: None,
                            dockerfile: Dockerfile::File { path: df.into() },
                            context_path: "/r".into(),
                        },
                        phase: "p".to_owned(),
                        source: "s".into(),
                    })
                    .collect(),
            })
            .collect();
        ImagesReport {
            cases,
            single: false,
            yaml: Vec::new(),
        }
    }

    fn tags(builds: &[(String, &Image, Cases<'_>)]) -> Vec<(String, Vec<(String, String)>)> {
        builds
            .iter()
            .map(|(tag, _, cases)| {
                (
                    tag.clone(),
                    cases
                        .iter()
                        .map(|(t, u)| ((*t).to_owned(), (*u).to_owned()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn builds_each_tag_once_with_the_cases_declaring_it() {
        let report = report(
            &[
                ("prod", "app", &[("a", "1"), ("b", "1")]),
                ("dev", "app", &[("a", "1"), ("b", "2")]),
            ],
            "/r/Dockerfile",
        );
        let pair = |t: &str, u: &str| (t.to_owned(), u.to_owned());
        assert_eq!(
            tags(&distinct(&report, &[]).unwrap()),
            [
                (
                    "a:1".to_owned(),
                    vec![pair("prod", "app"), pair("dev", "app")]
                ),
                ("b:1".to_owned(), vec![pair("prod", "app")]),
                ("b:2".to_owned(), vec![pair("dev", "app")]),
            ]
        );
        assert_eq!(
            tags(&distinct(&report, &["b".to_owned()]).unwrap())
                .into_iter()
                .map(|(tag, _)| tag)
                .collect::<Vec<_>>(),
            ["b:1", "b:2"]
        );
    }

    #[test]
    fn refuses_an_unknown_image_or_one_tag_two_ways() {
        let one = report(&[("prod", "app", &[("a", "1")])], "/r/Dockerfile");
        assert_eq!(
            distinct(&one, &["nope".to_owned()])
                .unwrap_err()
                .to_string(),
            "no selected unit declares image `nope` (they declare: a)"
        );
        let mut two = report(
            &[
                ("prod", "app", &[("a", "1")]),
                ("dev", "web", &[("a", "1")]),
            ],
            "/r/Dockerfile",
        );
        two.cases[1].images[0].image.dockerfile = Dockerfile::File {
            path: "/r/Other".into(),
        };
        assert_eq!(
            distinct(&two, &[]).unwrap_err().to_string(),
            "image `a:1` is declared differently by unit `web` on target `dev` and unit `app` on target `prod`: one tag must be one image"
        );
    }
}
