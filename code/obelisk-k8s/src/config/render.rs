//! Rendering one source of a phase ([`Source`]) for a case: each type turns the values it's given
//! into text. A values source's text is read as one values mapping; a manifests source's is k8s
//! resources. (An `inline` source isn't rendered: its values are used as they are.)
//!
//! - `gotmpl`: rendered by Helm through a throwaway chart, with the values as `.Values`.
//! - `file`: the file, as it is.
//! - `url`: fetched (`curl`), checked against `sha256` when it has one, and used as it is.
//! - `helm`: `helm template` of the chart as the case's release (CRDs included), with its values
//!   taken from the given values at the chart's `values` key (none without one).
//! - `command`: run from the repo root with the values (YAML) on stdin; its stdout is the text.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::ObeliskK8sError;
use crate::config::build::{HelmChart, Source, in_repo};
use crate::helm;

/// What a render gets: the values (as a tree, and as the YAML it's given), the release it renders
/// as, the chart version gotmpl templates see, and (for a chart) the cluster's capabilities.
pub(crate) struct Inputs<'a> {
    pub(crate) repo_root: &'a Path,
    /// Read only by a `helm` render (for its `values` key).
    pub(crate) values: &'a Map<String, Value>,
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
/// [`ObeliskK8sError::Io`] if a file can't be read; [`ObeliskK8sError::Usage`] if a chart's `values`
/// key isn't a mapping in the values.
pub(crate) fn text(
    source: &Source,
    shown: &Path,
    inputs: &Inputs<'_>,
) -> Result<String, ObeliskK8sError> {
    match source {
        Source::Inline { .. } => unreachable!("an inline source's values aren't rendered"),
        Source::Gotmpl { path, .. } => helm::render(
            &inputs.repo_root.join(path),
            shown,
            inputs.values_yaml,
            inputs.release,
            inputs.namespace,
            inputs.chart_version,
        ),
        Source::File { path, .. } => {
            std::fs::read_to_string(inputs.repo_root.join(path)).map_err(|source| {
                ObeliskK8sError::Io {
                    path: shown.to_owned(),
                    source,
                }
            })
        }
        Source::Url { url, sha256 } => fetch(url, sha256.as_deref()),
        Source::Helm(chart) => {
            let values = chart_values(chart, inputs.values)?;
            helm::template_chart(
                chart,
                inputs.release,
                inputs.namespace,
                values.as_deref(),
                inputs.capabilities,
            )
        }
        Source::Command(words) => run(words, inputs.repo_root, inputs.values_yaml),
    }
}

/// `chart`'s values: the subtree of `values` at its `values` key, as YAML.
fn chart_values(
    chart: &HelmChart,
    values: &Map<String, Value>,
) -> Result<Option<String>, ObeliskK8sError> {
    let Some(key) = &chart.values else {
        return Ok(None);
    };
    // Each part a key in the mapping the one before it found.
    let mut at: Option<&Value> = None;
    let mut map = Some(values);
    for part in key.split('.') {
        at = map.and_then(|m| m.get(part));
        map = match at {
            Some(Value::Object(inner)) => Some(inner),
            _ => None,
        };
    }
    let Some(subtree) = at else {
        return Err(ObeliskK8sError::Usage(format!(
            "chart `{}` takes its values from `{key}`, which isn't in the values",
            chart.chart
        )));
    };
    if !subtree.is_object() {
        return Err(ObeliskK8sError::Usage(format!(
            "chart `{}` takes its values from `{key}`, which isn't a mapping",
            chart.chart
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

/// Runs `words` (the program first) from `repo_root`, with `stdin` on its stdin.
fn run(words: &[String], repo_root: &Path, stdin: &str) -> Result<String, ObeliskK8sError> {
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
        let source = Source::File {
            path: PathBuf::from("x.yaml"),
            found: true,
        };
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
            "#!/bin/sh\nprintf 'seen: %s\\narg: %s\\n' \"$(grep -c 'unit: app')\" \"$1\"\n",
        );
        let words = vec![".obelisk-k8s/gen.sh".to_owned(), "prod".to_owned()];
        let values = values();
        let source = Source::Command(words);
        let text = text(&source, Path::new("-"), &inputs(dir.path(), &values)).unwrap();
        assert_eq!(text, "seen: 1\narg: prod\n");

        let error = run(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "echo boom >&2; exit 3".to_owned(),
            ],
            dir.path(),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("failed") && error.contains("boom"),
            "{error}"
        );
        let error = run(&["./nope".to_owned()], dir.path(), "")
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("can't run `./nope`"), "{error}");
    }

    #[test]
    fn a_chart_takes_its_values_from_its_key() {
        let chart = |values: Option<&str>| HelmChart {
            chart: "x".to_owned(),
            repo: None,
            version: "1".to_owned(),
            values: values.map(str::to_owned),
        };
        let values = values();
        assert_eq!(chart_values(&chart(None), &values).unwrap(), None);
        assert_eq!(
            chart_values(&chart(Some("charts.x")), &values)
                .unwrap()
                .as_deref(),
            Some("replicas: 2\n")
        );
        let error = chart_values(&chart(Some("charts.x.replicas")), &values)
            .unwrap_err()
            .to_string();
        assert!(error.ends_with("which isn't a mapping"), "{error}");
        for missing in ["charts.y", "charts.x.replicas.deeper", "nope"] {
            let error = chart_values(&chart(Some(missing)), &values)
                .unwrap_err()
                .to_string();
            assert!(error.contains("isn't in the values"), "{missing}: {error}");
        }
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
            values: Some("charts.x".to_owned()),
        };
        let values = values();
        let text = text(
            &Source::Helm(chart),
            Path::new("-"),
            &inputs(dir.path(), &values),
        )
        .unwrap();
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
