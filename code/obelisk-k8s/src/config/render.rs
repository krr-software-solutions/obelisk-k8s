//! Rendering one source of a phase ([`Source`]) for a case: each type turns the values it's given
//! into text. A values source's text is read as one values mapping; a manifests source's is k8s
//! resources. (An `inline` source isn't rendered: its values are used as they are.) A source with
//! an `unnestInput` is given only what's at that key, not all the values.
//!
//! - `gotmpl`: rendered by Helm through a throwaway chart, with the values as `.Values` (so what's
//!   at `unnestInput` must be a mapping).
//! - `file`: the file, as it is.
//! - `url`: fetched (`curl`), checked against `sha256` when it has one, and used as it is.
//! - `helm`: `helm template` of the chart as the case's release (CRDs included), with what's at its
//!   `unnestInput` (a mapping) as its values, or none without one.
//! - `command`: run from the repo root with the values (YAML; at `unnestInput`, any value) on
//!   stdin, and its `env` set over what it inherits; its stdout is the text.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::ObeliskK8sError;
use crate::config::build::{Source, SourceKind, in_repo};
use crate::helm;

/// What a render gets: the values (as a tree, and as the YAML it's given), the release it renders
/// as, the chart version gotmpl templates see, and (for a chart) the cluster's capabilities.
pub(crate) struct Inputs<'a> {
    pub(crate) repo_root: &'a Path,
    /// Read for a source's `unnestInput`.
    pub(crate) values: &'a Map<String, Value>,
    /// `values`, as YAML: what a source without an `unnestInput` gets (but a chart, which gets none).
    pub(crate) values_yaml: &'a str,
    pub(crate) release: &'a str,
    pub(crate) namespace: &'a str,
    pub(crate) chart_version: Option<&'a str>,
    /// The cluster a chart renders for (`apply`); without it, Helm's defaults (`template`).
    pub(crate) capabilities: Option<&'a helm::Capabilities>,
}

/// The text `source` (anything but `inline`) makes from `inputs`. `shown` is how the user should see
/// it.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if Helm fails (a `gotmpl` or `helm`); [`ObeliskK8sError::Render`] if a
/// URL can't be fetched or doesn't match its `sha256`, or a command can't run or fails;
/// [`ObeliskK8sError::Io`] if a file can't be read; [`ObeliskK8sError::Config`] if nothing is at
/// its `unnestInput`, or (a `gotmpl` or `helm`) what is isn't a mapping.
pub(crate) fn text(
    source: &Source,
    shown: &Path,
    inputs: &Inputs<'_>,
) -> Result<String, ObeliskK8sError> {
    let unnested = unnested_input(source, shown, inputs.values)?;
    let values_yaml = unnested.as_deref().unwrap_or(inputs.values_yaml);
    match &source.kind {
        SourceKind::Inline { .. } => unreachable!("an inline source's values aren't rendered"),
        SourceKind::Gotmpl { path, .. } => helm::render(
            &inputs.repo_root.join(path),
            shown,
            values_yaml,
            inputs.release,
            inputs.namespace,
            inputs.chart_version,
        ),
        SourceKind::File { path, .. } => std::fs::read_to_string(inputs.repo_root.join(path))
            .map_err(|source| ObeliskK8sError::Io {
                path: shown.to_owned(),
                source,
            }),
        SourceKind::Url { url, sha256 } => fetch(url, sha256.as_deref()),
        SourceKind::Helm(chart) => helm::template_chart(
            chart,
            inputs.release,
            inputs.namespace,
            unnested.as_deref(),
            inputs.capabilities,
        ),
        SourceKind::Command { command, env } => run(command, env, inputs.repo_root, values_yaml),
    }
}

/// What's at `source`'s `unnestInput` in `values`, as YAML; `None` without one.
fn unnested_input(
    source: &Source,
    shown: &Path,
    values: &Map<String, Value>,
) -> Result<Option<String>, ObeliskK8sError> {
    let Some(key) = &source.unnest_input else {
        return Ok(None);
    };
    let error = |message: String| ObeliskK8sError::Config {
        file: shown.to_owned(),
        at: None,
        message: format!("`unnestInput: {key}`: {message}"),
    };
    let subtree = key.get(values).map_err(error)?;
    // Helm's `.Values` (a template's, or a chart's) is a mapping.
    if matches!(source.kind, SourceKind::Gotmpl { .. } | SourceKind::Helm(_))
        && !subtree.is_object()
    {
        return Err(error(format!(
            "a `{}` source's values must be a mapping, and this isn't",
            source.type_name()
        )));
    }
    crate::yaml::to_string(subtree, Path::new(&format!("the values at `{key}`"))).map(Some)
}

/// Downloads `url` over HTTPS (redirects too), and checks it against `sha256`.
fn fetch(url: &str, sha256: Option<&str>) -> Result<String, ObeliskK8sError> {
    fetch_over(url, sha256, "=https")
}

/// [`fetch`], allowing `protocols` (curl's `--proto`): tests fetch `file://` URLs.
fn fetch_over(url: &str, sha256: Option<&str>, protocols: &str) -> Result<String, ObeliskK8sError> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--proto", protocols, "--proto-redir", protocols])
        .args(["--max-time", "120", url])
        .output()
        .map_err(|error| {
            ObeliskK8sError::Render(format!("can't run `curl` (is it installed?): {error}"))
        })?;
    if !output.status.success() {
        return Err(ObeliskK8sError::Render(format!(
            "couldn't fetch {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if let Some(expected) = sha256 {
        let actual = hex(&Sha256::digest(&output.stdout));
        if actual != expected {
            return Err(ObeliskK8sError::Render(format!(
                "{url} doesn't match its sha256: expected {expected}, got {actual}"
            )));
        }
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ObeliskK8sError::Render(format!("{url} isn't UTF-8 text")))
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Runs `words` (the program first) from `repo_root`, with `env` set over what it inherits and
/// `stdin` on its stdin.
fn run(
    words: &[String],
    env: &BTreeMap<String, String>,
    repo_root: &Path,
    stdin: &str,
) -> Result<String, ObeliskK8sError> {
    // A path (with a `/`) is absolute, or relative to the repo root; a bare name is looked up on
    // `PATH`. Shown relative to the repo root.
    let program = if words[0].contains('/') {
        repo_root.join(&words[0])
    } else {
        words[0].clone().into()
    };
    let mut shown_words = words.to_vec();
    shown_words[0] = in_repo(repo_root, Path::new(&words[0]))
        .display()
        .to_string();
    let shown = shown_words.join(" ");
    let mut child = Command::new(&program)
        .args(&words[1..])
        .envs(env)
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ObeliskK8sError::Render(format!("can't run `{shown}`: {error}")))?;
    // Written from a thread, so a command that writes a lot before reading can't deadlock us.
    let mut input = child.stdin.take();
    let stdin = stdin.to_owned();
    let writer = std::thread::spawn(move || {
        if let Some(input) = input.as_mut() {
            // A command that doesn't read its stdin closes the pipe: that's fine.
            let _ = input.write_all(stdin.as_bytes());
        }
    });
    let output = child
        .wait_with_output()
        .map_err(|error| ObeliskK8sError::Render(format!("`{shown}` failed: {error}")))?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(ObeliskK8sError::Render(format!(
            "`{shown}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        ObeliskK8sError::Render(format!("`{shown}` printed something that isn't UTF-8"))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::build::HelmChart;
    use crate::config::key_path::KeyPath;
    use crate::config::test_support::{helm_installed, write_script};

    fn values() -> Map<String, Value> {
        let Value::Object(map) = serde_json::json!({
            "obelisk": { "unit": "app" },
            "charts": { "x": { "replicas": 2 } },
        }) else {
            unreachable!()
        };
        map
    }

    fn inputs<'a>(repo_root: &'a Path, values: &'a Map<String, Value>) -> Inputs<'a> {
        Inputs {
            repo_root,
            values,
            values_yaml: "obelisk:\n  unit: app\n",
            release: "app",
            namespace: "ns",
            chart_version: None,
            capabilities: None,
        }
    }

    #[test]
    fn a_file_is_used_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.yaml"), "a: {{ not templated }}\n").unwrap();
        let source = Source::new(SourceKind::File {
            path: PathBuf::from("x.yaml"),
            found: true,
        });
        let values = values();
        let text = text(&source, Path::new("x.yaml"), &inputs(dir.path(), &values)).unwrap();
        assert_eq!(text, "a: {{ not templated }}\n");
    }

    #[test]
    fn fetches_a_url_and_checks_its_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("remote.yaml");
        std::fs::write(&file, "kind: X\n").unwrap();
        let url = format!("file://{}", file.display());
        // sha256("kind: X\n")
        let sha = hex(&Sha256::digest(b"kind: X\n"));
        let file_ok = |url: &str, sha: Option<&str>| fetch_over(url, sha, "=file");
        assert_eq!(file_ok(&url, Some(&sha)).unwrap(), "kind: X\n");
        assert_eq!(file_ok(&url, None).unwrap(), "kind: X\n");
        let error = file_ok(&url, Some(&"0".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("doesn't match its sha256"), "{error}");
        let error = file_ok(&format!("{url}.nope"), None)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("couldn't fetch"), "{error}");
        // The real thing only speaks https.
        let error = fetch(&url, None).unwrap_err().to_string();
        assert!(error.starts_with("couldn't fetch"), "{error}");
    }

    #[test]
    fn runs_a_command_with_the_values_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        write_script(
            &dir.path().join(".obelisk-k8s/gen.sh"),
            "#!/bin/sh\nprintf 'seen: %s\\narg: %s\\nmode: %s\\n' \"$(grep -c 'unit: app')\" \"$1\" \"$MODE\"\n",
        );
        let words = vec![".obelisk-k8s/gen.sh".to_owned(), "prod".to_owned()];
        let values = values();
        let source = Source::new(SourceKind::Command {
            command: words,
            env: BTreeMap::from([("MODE".to_owned(), "fast".to_owned())]),
        });
        let text = text(&source, Path::new("-"), &inputs(dir.path(), &values)).unwrap();
        assert_eq!(text, "seen: 1\narg: prod\nmode: fast\n");

        let error = run(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "echo boom >&2; exit 3".to_owned(),
            ],
            &BTreeMap::new(),
            dir.path(),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("failed") && error.contains("boom"),
            "{error}"
        );
        let error = run(&["./nope".to_owned()], &BTreeMap::new(), dir.path(), "")
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("can't run `./nope`"), "{error}");
    }

    #[test]
    fn a_source_sees_only_what_is_at_its_unnest_input() {
        let dir = tempfile::tempdir().unwrap();
        let values = values();
        let cat = |unnest: Option<&str>| Source {
            unnest_input: unnest.map(|key| KeyPath::parse(key).unwrap()),
            ..Source::new(SourceKind::Command {
                command: vec!["cat".to_owned()],
                env: BTreeMap::new(),
            })
        };
        let text = |source: &Source| text(source, Path::new("-"), &inputs(dir.path(), &values));
        // Without one, all the values; with one, what's there, whatever it is.
        assert_eq!(text(&cat(None)).unwrap(), "obelisk:\n  unit: app\n");
        assert_eq!(text(&cat(Some("charts.x"))).unwrap(), "replicas: 2\n");
        assert_eq!(text(&cat(Some("charts.x.replicas"))).unwrap(), "2\n");
        for (key, expected) in [
            ("charts.y", "no `y` under `charts`"),
            (
                "charts.x.replicas.deeper",
                "`charts.x.replicas` isn't a mapping",
            ),
            ("nope", "no `nope` in the values"),
        ] {
            assert_eq!(
                text(&cat(Some(key))).unwrap_err().to_string(),
                format!("-: `unnestInput: {key}`: {expected}")
            );
        }
        // A template's (or a chart's) values are a mapping.
        let gotmpl = Source {
            unnest_input: Some(KeyPath::parse("charts.x.replicas").unwrap()),
            ..Source::new(SourceKind::Gotmpl {
                path: PathBuf::from("t.gotmpl"),
                found: true,
            })
        };
        assert_eq!(
            text(&gotmpl).unwrap_err().to_string(),
            "-: `unnestInput: charts.x.replicas`: a `gotmpl` source's values must be a mapping, and this isn't"
        );
    }

    #[test]
    fn templates_a_chart_as_the_release() {
        if !helm_installed() {
            return;
        }
        // A local chart stands in for a repo's (`helm template` takes either).
        let dir = tempfile::tempdir().unwrap();
        let chart_dir = dir.path().join("c");
        std::fs::create_dir_all(chart_dir.join("templates")).unwrap();
        std::fs::create_dir_all(chart_dir.join("crds")).unwrap();
        std::fs::write(
            chart_dir.join("Chart.yaml"),
            "apiVersion: v2\nname: c\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(
            chart_dir.join("templates/cm.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\n  namespace: {{ .Release.Namespace }}\ndata:\n  replicas: {{ .Values.replicas | quote }}\n",
        )
        .unwrap();
        std::fs::write(
            chart_dir.join("crds/crd.yaml"),
            "kind: CustomResourceDefinition\n",
        )
        .unwrap();
        let chart = HelmChart {
            chart: chart_dir.display().to_string(),
            repo: None,
            version: "1.0.0".to_owned(),
        };
        let values = values();
        let source = Source {
            unnest_input: Some(KeyPath::parse("charts.x").unwrap()),
            ..Source::new(SourceKind::Helm(chart))
        };
        let text = text(&source, Path::new("-"), &inputs(dir.path(), &values)).unwrap();
        assert!(
            text.contains("kind: CustomResourceDefinition"),
            "CRDs included: {text}"
        );
        assert!(
            text.contains("  name: app\n  namespace: ns\ndata:\n  replicas: \"2\""),
            "{text}"
        );
    }
}
