//! Running Helm: obelisk-k8s renders Go templates through a throwaway chart, so they get exactly Helm's
//! functions and objects.

use std::path::Path;
use std::process::Command;

use crate::ObeliskK8sError;

/// Helm's limit on release names.
const RELEASE_NAME_MAX: usize = 53;

/// `name` is what files will render as a Helm release named: Helm wants at most 53 characters. `kind`
/// says what it names (e.g. `unit`), `what` what renders (e.g. `generators`).
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if it's too long.
pub(crate) fn check_release_name(
    name: &str,
    kind: &str,
    what: &str,
) -> Result<(), ObeliskK8sError> {
    if name.len() > RELEASE_NAME_MAX {
        return Err(ObeliskK8sError::Usage(format!(
            "{kind} `{name}` has {what}, which render as a Helm release named after it, but Helm release names are at most {RELEASE_NAME_MAX} characters"
        )));
    }
    Ok(())
}

/// Whether `version` is a chart version Helm takes: `SemVer`, loosely (an optional `v`, one to three
/// numeric parts without leading zeros, then optional `-prerelease` and `+build` parts, where a
/// numeric prerelease part can't have a leading zero either).
#[must_use]
pub(crate) fn is_chart_version(version: &str) -> bool {
    let rest = version.strip_prefix('v').unwrap_or(version);
    let (rest, build) = rest
        .split_once('+')
        .map_or((rest, None), |(r, b)| (r, Some(b)));
    let (core, pre) = rest
        .split_once('-')
        .map_or((rest, None), |(c, p)| (c, Some(p)));
    let numeric = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    let identifier =
        |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let identifiers = |s: &str| !s.is_empty() && s.split('.').all(identifier);
    // A numeric prerelease part can't have a leading zero (build metadata can).
    let prerelease = |s: &str| {
        identifiers(s)
            && s.split('.')
                .all(|p| !p.bytes().all(|b| b.is_ascii_digit()) || numeric(p))
    };
    let parts: Vec<&str> = core.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts.iter().all(|p| numeric(p))
        && pre.is_none_or(prerelease)
        && build.is_none_or(identifiers)
}

/// The throwaway chart's name; Helm names templates `<chart>/templates/<file>` in its errors.
const CHART: &str = "obelisk-render";
const TEMPLATE: &str = "templates/template.yaml";

/// Renders the Go template at `template` (shown as `shown`) with `values_yaml` as `.Values`, as release
/// `release` in namespace `namespace`, in a chart whose `version` and `appVersion` are `chart_version`
/// (`0.0.0` without one), and returns what it printed (`---`-separated documents).
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run or fails (its message, with the template named
/// as `shown`); [`ObeliskK8sError::Io`] if the template can't be read or the chart can't be written.
pub(crate) fn render(
    template: &Path,
    shown: &Path,
    values_yaml: &str,
    release: &str,
    namespace: &str,
    chart_version: Option<&str>,
) -> Result<String, ObeliskK8sError> {
    let io_error = |source| ObeliskK8sError::Io {
        path: shown.to_owned(),
        source,
    };
    let text = std::fs::read_to_string(template).map_err(io_error)?;
    let chart = tempfile::tempdir().map_err(io_error)?;
    let write = |path: &str, contents: &str| {
        let path = chart.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap_or(chart.path()))
            .and_then(|()| std::fs::write(path, contents))
            .map_err(io_error)
    };
    let version = chart_version.unwrap_or("0.0.0");
    let chart_yaml = crate::yaml::to_string(
        &serde_json::json!({ "apiVersion": "v2", "name": CHART, "version": version, "appVersion": version }),
        Path::new("Chart.yaml"),
    )?;
    write("Chart.yaml", &chart_yaml)?;
    write("values.yaml", values_yaml)?;
    write(TEMPLATE, &text)?;

    let output = Command::new("helm")
        .arg("template")
        .arg(release)
        .arg(chart.path())
        .args(["--namespace", namespace])
        .output()
        .map_err(|error| {
            ObeliskK8sError::Helm(format!("can't run `helm` (is it installed?): {error}"))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr
            .replace(&format!("{CHART}/{TEMPLATE}"), &shown.display().to_string())
            .replace(&chart.path().display().to_string(), "<chart>");
        return Err(ObeliskK8sError::Helm(format!(
            "{}: helm couldn't render it: {}",
            shown.display(),
            helm_message(&stderr)
        )));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        ObeliskK8sError::Helm(format!(
            "{}: helm printed something that isn't UTF-8",
            shown.display()
        ))
    })?;
    // Helm's `# Source:` comments name the throwaway chart's file: name the real one. Anchored to the
    // `---` line Helm always puts before them, so the same text in a value isn't touched.
    Ok(text.replace(
        &format!("---\n# Source: {CHART}/{TEMPLATE}\n"),
        &format!("---\n# Source: {}\n", shown.display()),
    ))
}

/// A chart to install: from a chart repository, or an OCI reference.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Chart<'a> {
    Repo {
        url: &'a str,
        name: &'a str,
    },
    Oci(&'a str),
    /// A chart directory on disk.
    Dir(&'a Path),
}

/// Where and how to install a release.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Install<'a> {
    pub(crate) release: &'a str,
    pub(crate) chart: Chart<'a>,
    /// For a repo or OCI chart.
    pub(crate) version: Option<&'a str>,
    /// Created if it's missing.
    pub(crate) namespace: &'a str,
    /// The kubectl context to install on (never the current one by default).
    pub(crate) context: &'a str,
}

/// How long Helm waits for a release to be ready.
pub(crate) const TIMEOUT: &str = "5m";

/// `helm upgrade --install`s `install` and waits for it to be ready; if it doesn't get there, rolls
/// it back to its last good revision (or removes it, on a first install), so a re-run starts clean.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run or fails, or the release is stuck mid-operation
/// (e.g. an earlier run was interrupted).
pub(crate) fn upgrade_install(install: &Install<'_>) -> Result<(), ObeliskK8sError> {
    if let Some(status) = pending(install)? {
        return Err(ObeliskK8sError::Helm(format!(
            "release `{release}` on `{context}` is stuck `{status}` (an earlier run was interrupted?): `helm --kube-context {context} --namespace {namespace} rollback {release}` (or `uninstall`), then retry",
            release = install.release,
            context = install.context,
            namespace = install.namespace,
        )));
    }
    let args = upgrade_args(install);
    let output = run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if !output.status.success() {
        return Err(ObeliskK8sError::Helm(format!(
            "installing `{}` on `{}` failed: {}",
            install.release,
            install.context,
            helm_message(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(())
}

fn upgrade_args(install: &Install<'_>) -> Vec<String> {
    let mut args = vec!["upgrade", "--install", install.release];
    let dir;
    match install.chart {
        Chart::Repo { url, name } => args.extend([name, "--repo", url]),
        Chart::Oci(reference) => args.push(reference),
        Chart::Dir(path) => {
            dir = path.display().to_string();
            args.push(&dir);
        }
    }
    if let Some(version) = install.version {
        args.extend(["--version", version]);
    }
    args.extend(["--namespace", install.namespace, "--create-namespace"]);
    args.extend(["--kube-context", install.context]);
    args.extend(["--wait", "--rollback-on-failure", "--timeout", TIMEOUT]);
    args.into_iter().map(str::to_owned).collect()
}

/// The release's status, if it's stuck mid-operation (`pending-install`, `pending-upgrade`, ...).
fn pending(install: &Install<'_>) -> Result<Option<String>, ObeliskK8sError> {
    let output = run(&[
        "status",
        install.release,
        "--namespace",
        install.namespace,
        "--kube-context",
        install.context,
        "--output",
        "json",
    ])?;
    if !output.status.success() {
        // No such release yet.
        return Ok(None);
    }
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    Ok(status["info"]["status"]
        .as_str()
        .filter(|s| s.starts_with("pending-"))
        .map(str::to_owned))
}

/// Writes a chart that installs `manifests` (plain YAML) as-is, into `dir`: shipped as a file and
/// printed with `.Files.Get`, so nothing in it is read as a Go template.
///
/// # Errors
///
/// [`ObeliskK8sError::Io`] if it can't be written; [`ObeliskK8sError::Yaml`] if `Chart.yaml` can't.
pub(crate) fn write_manifests_chart(
    dir: &Path,
    name: &str,
    manifests: &str,
) -> Result<(), ObeliskK8sError> {
    let io_error = |source| ObeliskK8sError::Io {
        path: format!("<chart for {name}>").into(),
        source,
    };
    let chart_yaml = crate::yaml::to_string(
        &serde_json::json!({ "apiVersion": "v2", "name": name, "version": "0.0.0" }),
        Path::new("Chart.yaml"),
    )?;
    std::fs::create_dir_all(dir.join("templates")).map_err(io_error)?;
    std::fs::write(dir.join("Chart.yaml"), chart_yaml).map_err(io_error)?;
    std::fs::write(dir.join("manifests.yaml"), manifests).map_err(io_error)?;
    std::fs::write(dir.join("templates/manifests.yaml"), MANIFESTS_TEMPLATE).map_err(io_error)
}

const MANIFESTS_TEMPLATE: &str = "{{ .Files.Get \"manifests.yaml\" }}\n";

fn run(args: &[&str]) -> Result<std::process::Output, ObeliskK8sError> {
    Command::new("helm").args(args).output().map_err(|error| {
        ObeliskK8sError::Helm(format!("can't run `helm` (is it installed?): {error}"))
    })
}

/// What Helm said, without its own framing: an `Error: ` prefix, and a hint about `--debug`.
fn helm_message(stderr: &str) -> String {
    stderr
        .trim()
        .trim_start_matches("Error: ")
        .lines()
        .filter(|l| !l.starts_with("Use --debug flag"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_on_the_given_context_and_rolls_back_on_failure() {
        let args = upgrade_args(&Install {
            release: "metallb",
            chart: Chart::Repo {
                url: "https://example.org/charts",
                name: "metallb",
            },
            version: Some("0.16.1"),
            namespace: "metallb-system",
            context: "k3d-c",
        });
        assert_eq!(
            args.join(" "),
            "upgrade --install metallb metallb --repo https://example.org/charts --version 0.16.1 \
             --namespace metallb-system --create-namespace --kube-context k3d-c \
             --wait --rollback-on-failure --timeout 5m"
        );
        let args = upgrade_args(&Install {
            release: "base",
            chart: Chart::Dir(Path::new("/tmp/chart")),
            version: None,
            namespace: "ns",
            context: "k3d-c",
        });
        assert!(
            args.join(" ")
                .starts_with("upgrade --install base /tmp/chart --namespace ns ")
        );
    }

    #[test]
    fn a_manifests_chart_prints_its_manifests_untemplated() {
        let dir = tempfile::tempdir().unwrap();
        write_manifests_chart(dir.path(), "base", "a: '{{ not a template }}'\n").unwrap();
        let read = |p: &str| std::fs::read_to_string(dir.path().join(p)).unwrap();
        assert!(
            read("Chart.yaml").contains("name: base"),
            "{}",
            read("Chart.yaml")
        );
        assert_eq!(read("manifests.yaml"), "a: '{{ not a template }}'\n");
        assert_eq!(read("templates/manifests.yaml"), MANIFESTS_TEMPLATE);
    }

    #[test]
    fn strips_helms_framing() {
        assert_eq!(
            helm_message("Error: boom\nUse --debug flag to render out invalid YAML\n"),
            "boom"
        );
    }

    #[test]
    fn knows_chart_versions() {
        for ok in [
            "0.0.0",
            "1.2.3",
            "v1.2.3",
            "1.2",
            "1",
            "0.0.0-b84d8f6",
            "v1.2.3-4-gb84d8f6-dirty",
            "1.2.3+dirty",
            "1.2.3-rc.1+build.5",
            "1.2.3+01",
            "0.0.0-a123456",
        ] {
            assert!(is_chart_version(ok), "{ok}");
        }
        for bad in [
            "",
            "b84d8f6",
            "0b84d8f6",
            "main",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "v",
            "1.2.3 ",
            "1.2.3-01",
            "1.2.3-rc.01",
            "0.0.0-0123456",
        ] {
            assert!(!is_chart_version(bad), "{bad}");
        }
    }
}
