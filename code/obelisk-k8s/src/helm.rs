//! Running Helm: obelisk-k8s renders Go templates through a throwaway chart, so they get exactly Helm's
//! functions and objects.

use std::path::Path;
use std::process::Command;

use crate::ObeliskK8sError;

/// Helm's limit on release names.
pub(crate) const RELEASE_NAME_MAX: usize = 53;

/// Whether Helm takes `name` as a release name, length-wise (at most [`RELEASE_NAME_MAX`]).
#[must_use]
pub(crate) fn fits_release_name(name: &str) -> bool {
    name.len() <= RELEASE_NAME_MAX
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

/// What a cluster can do, for charts that render by it (`.Capabilities`): its Kubernetes version and
/// API versions. Without them, `helm template` assumes its own defaults, and a chart that renders a
/// resource only if the cluster has its API (a `ServiceMonitor`, an `HTTPRoute`) leaves it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub(crate) kube_version: String,
    pub(crate) api_versions: Vec<String>,
}

/// The capabilities of the cluster at kubectl context `context`.
///
/// # Errors
///
/// [`ObeliskK8sError::Kubectl`] if `kubectl` can't be run, fails (e.g. the cluster isn't up), or
/// prints something unexpected.
pub(crate) fn capabilities(context: &str) -> Result<Capabilities, ObeliskK8sError> {
    let kubectl = |args: &[&str]| -> Result<String, ObeliskK8sError> {
        let output = Command::new("kubectl")
            .args(["--context", context])
            .args(args)
            .output()
            .map_err(|error| {
                ObeliskK8sError::Kubectl(format!("can't run `kubectl` (is it installed?): {error}"))
            })?;
        if !output.status.success() {
            return Err(ObeliskK8sError::Kubectl(format!(
                "couldn't ask `{context}` what it can do (`kubectl {}`): {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let version: serde_json::Value =
        serde_json::from_str(&kubectl(&["version", "--output", "json"])?).map_err(|error| {
            ObeliskK8sError::Kubectl(format!(
                "`kubectl version` printed something unexpected: {error}"
            ))
        })?;
    let kube_version = version["serverVersion"]["gitVersion"]
        .as_str()
        .ok_or_else(|| ObeliskK8sError::Kubectl(format!("`{context}` didn't say its version")))?
        .to_owned();
    let api_versions = kubectl(&["api-versions"])?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Capabilities {
        kube_version,
        api_versions,
    })
}

/// `helm template`s `chart` as release `release` in `namespace`, with `values` (YAML) as its values,
/// CRDs included, for a cluster with `capabilities` (Helm's defaults without), and returns what it
/// printed.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run or fails; [`ObeliskK8sError::Io`] if the values
/// can't be written.
pub(crate) fn template_chart(
    chart: &crate::config::build::HelmChart,
    release: &str,
    namespace: &str,
    values: Option<&str>,
    capabilities: Option<&Capabilities>,
) -> Result<String, ObeliskK8sError> {
    let dir = tempfile::tempdir().map_err(|source| ObeliskK8sError::Io {
        path: format!("<values for {}>", chart.chart).into(),
        source,
    })?;
    let values_file = dir.path().join("values.yaml");
    if let Some(values) = values {
        std::fs::write(&values_file, values).map_err(|source| ObeliskK8sError::Io {
            path: format!("<values for {}>", chart.chart).into(),
            source,
        })?;
    }
    let args = template_args(
        chart,
        release,
        namespace,
        values.map(|_| values_file.as_path()),
        capabilities,
    );
    let output = run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if !output.status.success() {
        return Err(ObeliskK8sError::Helm(format!(
            "chart `{}` {}: helm couldn't render it: {}",
            chart.chart,
            chart.version,
            helm_message(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        ObeliskK8sError::Helm(format!(
            "chart `{}`: helm printed something that isn't UTF-8",
            chart.chart
        ))
    })
}

fn template_args(
    chart: &crate::config::build::HelmChart,
    release: &str,
    namespace: &str,
    values: Option<&Path>,
    capabilities: Option<&Capabilities>,
) -> Vec<String> {
    let mut args: Vec<String> = ["template", release, &chart.chart]
        .map(str::to_owned)
        .into();
    if let Some(repo) = &chart.repo {
        args.extend(["--repo".to_owned(), repo.clone()]);
    }
    args.extend(
        [
            "--version",
            &chart.version,
            "--namespace",
            namespace,
            "--include-crds",
        ]
        .map(str::to_owned),
    );
    if let Some(values) = values {
        args.extend(["--values".to_owned(), values.display().to_string()]);
    }
    if let Some(capabilities) = capabilities {
        args.extend([
            "--kube-version".to_owned(),
            capabilities.kube_version.clone(),
        ]);
        for api in &capabilities.api_versions {
            args.extend(["--api-versions".to_owned(), api.clone()]);
        }
    }
    args
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
    /// What it may take over from what's already in the cluster.
    pub(crate) takeover: Takeover,
}

/// What an install may take over from what's already in the cluster.
#[derive(Debug, Clone, Copy, Default)]
pub struct Takeover {
    /// `--take-ownership`: adopt resources that already exist but aren't the release's (e.g. ones
    /// obelisk-local applied; Helm doesn't check, so another release's too). A failed install then
    /// isn't rolled back: the revision it would go back to doesn't have what was just adopted, so
    /// Helm would delete it (on a first install, by uninstalling the release). A release left
    /// failed isn't rolled back by later installs either.
    pub take_ownership: bool,
    /// `--force-conflicts`, with server-side apply forced on (it's ignored client-side, which a
    /// release's earlier revision may have used): take fields another field manager owns. One-off:
    /// left on, it fights what the cluster manages (an HPA's `spec.replicas`). The release stays
    /// server-side after (Helm's default follows the previous revision's method).
    pub force_conflicts: bool,
}

/// How long Helm waits for a release to be ready.
pub(crate) const TIMEOUT: &str = "5m";

/// `helm upgrade --install`s `install` and waits for it to be ready; if it doesn't get there, rolls
/// it back to its last good revision (or removes it, on a first install), so a re-run starts clean.
/// Not when it takes ownership ([`Takeover::take_ownership`]), nor when the release is already
/// failed (taking ownership leaves one so; so can a rollback that failed, or Helm run by hand):
/// either way the revision it would go back to may lack what was adopted, and rolling back would
/// delete it. It's left failed for a re-run to fix.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run or fails, the release's status can't be read,
/// or the release is stuck mid-operation (e.g. an earlier run was interrupted).
pub(crate) fn upgrade_install(install: &Install<'_>) -> Result<(), ObeliskK8sError> {
    let status = status(install)?;
    if let Some(status) = status.as_deref().filter(|s| s.starts_with("pending-")) {
        return Err(ObeliskK8sError::Helm(format!(
            "release `{release}` on `{context}` is stuck `{status}` (an earlier run was interrupted?): `helm --kube-context {context} --namespace {namespace} rollback {release}` (or `uninstall`), then retry; after an interrupted `--take-ownership` run, either deletes what it adopted (`rollback` deletes what's not in the revision it goes back to; `uninstall`, everything)",
            release = install.release,
            context = install.context,
            namespace = install.namespace,
        )));
    }
    let args = upgrade_args(install, status.as_deref());
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

/// Whether a failed install should roll back, given the release's current `status` (see
/// [`upgrade_install`]).
fn rolls_back(takeover: Takeover, status: Option<&str>) -> bool {
    !takeover.take_ownership && status != Some("failed")
}

/// `status`: the release's current one, for [`rolls_back`].
fn upgrade_args(install: &Install<'_>, status: Option<&str>) -> Vec<String> {
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
    args.push("--wait");
    if install.takeover.take_ownership {
        args.push("--take-ownership");
    }
    if rolls_back(install.takeover, status) {
        args.push("--rollback-on-failure");
    }
    if install.takeover.force_conflicts {
        args.extend(["--server-side", "true", "--force-conflicts"]);
    }
    args.extend(["--timeout", TIMEOUT]);
    args.into_iter().map(str::to_owned).collect()
}

/// The release's status (`deployed`, `failed`, `pending-upgrade`, ...), if it exists.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run, or fails other than by not finding the release
/// (a release mistaken for missing would be rolled back on failure; see [`upgrade_install`]).
fn status(install: &Install<'_>) -> Result<Option<String>, ObeliskK8sError> {
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("release: not found") {
            // No such release yet.
            return Ok(None);
        }
        return Err(ObeliskK8sError::Helm(format!(
            "can't read the status of release `{}` on `{}`: {}",
            install.release,
            install.context,
            helm_message(&stderr)
        )));
    }
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    Ok(status["info"]["status"].as_str().map(str::to_owned))
}

/// `helm upgrade --install`s `manifests` (plain YAML) as release `release` at chart version
/// `version`, through a throwaway chart ([`write_manifests_chart`]), into `namespace` on `context`,
/// taking over what `takeover` allows; see [`upgrade_install`].
///
/// # Errors
///
/// As [`upgrade_install`]; [`ObeliskK8sError::Io`] if the chart can't be written.
pub(crate) fn install_manifests(
    release: &str,
    version: &str,
    manifests: &str,
    namespace: &str,
    context: &str,
    takeover: Takeover,
) -> Result<(), ObeliskK8sError> {
    let chart = manifests_chart(release, version, manifests)?;
    upgrade_install(&Install {
        release,
        chart: Chart::Dir(chart.path()),
        version: None,
        namespace,
        context,
        takeover,
    })
}

/// A throwaway chart ([`write_manifests_chart`]) in a temporary directory, deleted when it's dropped.
///
/// # Errors
///
/// As [`write_manifests_chart`]; [`ObeliskK8sError::Io`] if the directory can't be made.
pub(crate) fn manifests_chart(
    name: &str,
    version: &str,
    manifests: &str,
) -> Result<tempfile::TempDir, ObeliskK8sError> {
    let chart = tempfile::tempdir().map_err(|source| ObeliskK8sError::Io {
        path: format!("<chart for {name}>").into(),
        source,
    })?;
    write_manifests_chart(chart.path(), name, version, manifests)?;
    Ok(chart)
}

/// Writes a chart that installs `manifests` (plain YAML) as-is, into `dir`: shipped as a file and
/// printed with `.Files.Get`, so nothing in it is read as a Go template. The chart's `version` and
/// `appVersion` are `version`.
///
/// # Errors
///
/// [`ObeliskK8sError::Io`] if it can't be written; [`ObeliskK8sError::Yaml`] if `Chart.yaml` can't.
pub(crate) fn write_manifests_chart(
    dir: &Path,
    name: &str,
    version: &str,
    manifests: &str,
) -> Result<(), ObeliskK8sError> {
    let io_error = |source| ObeliskK8sError::Io {
        path: format!("<chart for {name}>").into(),
        source,
    };
    let chart_yaml = crate::yaml::to_string(
        &serde_json::json!({ "apiVersion": "v2", "name": name, "version": version, "appVersion": version }),
        Path::new("Chart.yaml"),
    )?;
    std::fs::create_dir_all(dir.join("templates")).map_err(io_error)?;
    std::fs::write(dir.join("Chart.yaml"), chart_yaml).map_err(io_error)?;
    std::fs::write(dir.join("manifests.yaml"), manifests).map_err(io_error)?;
    std::fs::write(dir.join("templates/manifests.yaml"), MANIFESTS_TEMPLATE).map_err(io_error)
}

const MANIFESTS_TEMPLATE: &str = "{{ .Files.Get \"manifests.yaml\" }}\n";

/// What `diff` says to do without the helm-diff plugin.
pub(crate) const DIFF_PLUGIN_MISSING: &str = "`obelisk-k8s diff` needs the helm-diff plugin: `helm plugin install https://github.com/databus23/helm-diff`";

/// A release to diff against what's installed, with the helm-diff plugin (`helm diff upgrade`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Diff<'a> {
    pub(crate) release: &'a str,
    /// A chart directory on disk (e.g. [`write_manifests_chart`]'s).
    pub(crate) chart: &'a Path,
    pub(crate) namespace: &'a str,
    /// The kubectl context the release is on (never the current one by default).
    pub(crate) context: &'a str,
    pub(crate) options: DiffOptions,
}

/// How to diff, beyond what `apply` would install.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffOptions {
    /// As `apply --take-ownership` would: resources already in the cluster that aren't the
    /// release's diff as adopted, not refused.
    pub take_ownership: bool,
    /// Print Secrets' data as it is (helm-diff masks it otherwise).
    pub show_secrets: bool,
}

/// Checks the helm-diff plugin is installed (`helm diff version`).
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run, the plugin isn't there (saying how to install
/// it), or it fails otherwise (saying what it said).
pub(crate) fn check_diff_plugin() -> Result<(), ObeliskK8sError> {
    let output = run(&["diff", "version"])?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ObeliskK8sError::Helm(if diff_plugin_missing(&stderr) {
        DIFF_PLUGIN_MISSING.to_owned()
    } else {
        format!(
            "the helm-diff plugin doesn't run (`helm diff version`): {}",
            helm_message(&stderr)
        )
    }))
}

/// `helm diff upgrade`s `diff`: what `helm upgrade --install` of it would change, computed against
/// what's live in the cluster (a three-way merge), without changing anything. Returns the diff as
/// helm-diff printed it: empty when nothing would change.
///
/// # Errors
///
/// [`ObeliskK8sError::Helm`] if `helm` can't be run, the plugin isn't there, or it fails.
pub(crate) fn diff_upgrade(diff: &Diff<'_>) -> Result<String, ObeliskK8sError> {
    let output = Command::new("helm")
        .args(diff_args(diff))
        // Captured, so helm-diff wouldn't color it; the CLI drops colors where they don't belong.
        // (Not `--color`: Helm 4 takes that as its own, with a value.)
        .env("HELM_DIFF_COLOR", "true")
        .output()
        .map_err(|error| {
            ObeliskK8sError::Helm(format!("can't run `helm` (is it installed?): {error}"))
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if diff_plugin_missing(&stderr) {
        return Err(ObeliskK8sError::Helm(DIFF_PLUGIN_MISSING.to_owned()));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        ObeliskK8sError::Helm(format!(
            "diffing `{}` on `{}`: helm printed something that isn't UTF-8",
            diff.release, diff.context
        ))
    })?;
    // Not `--detailed-exitcode`: whether it printed a diff says as much, and its 2 for changes is
    // also what a Go panic exits with.
    if !output.status.success() {
        return Err(ObeliskK8sError::Helm(format!(
            "diffing `{}` on `{}` failed: {}",
            diff.release,
            diff.context,
            helm_message(&stderr)
        )));
    }
    Ok(text)
}

/// Server-side apply isn't forced, unlike `apply --force-conflicts`: helm-diff has no
/// `--force-conflicts`, and a forced server-side dry run without it would fail on the very conflicts
/// `apply` would take.
fn diff_args(diff: &Diff<'_>) -> Vec<String> {
    let chart = diff.chart.display().to_string();
    let mut args = vec![
        "diff",
        "upgrade",
        diff.release,
        &chart,
        "--namespace",
        diff.namespace,
        "--kube-context",
        diff.context,
        "--allow-unreleased",
        "--three-way-merge",
        "--normalize-manifests",
    ];
    let options = diff.options;
    if options.take_ownership {
        args.push("--take-ownership");
    }
    if options.show_secrets {
        args.push("--show-secrets");
    }
    args.into_iter().map(str::to_owned).collect()
}

/// Whether Helm's `stderr` says it has no `diff` command (the helm-diff plugin isn't installed).
fn diff_plugin_missing(stderr: &str) -> bool {
    stderr.contains("unknown command \"diff\"")
}

fn run(args: &[&str]) -> Result<std::process::Output, ObeliskK8sError> {
    Command::new("helm").args(args).output().map_err(|error| {
        ObeliskK8sError::Helm(format!("can't run `helm` (is it installed?): {error}"))
    })
}

/// What Helm said, without its own framing: an `Error: ` prefix, a hint about `--debug`, and the
/// line it adds when a plugin (helm-diff) fails.
fn helm_message(stderr: &str) -> String {
    stderr
        .trim()
        .trim_start_matches("Error: ")
        .lines()
        .filter(|l| {
            let plugin_failed =
                l.starts_with("Error: plugin \"") && l.ends_with("\" exited with error");
            !l.starts_with("Use --debug flag") && !plugin_failed
        })
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
        let args = upgrade_args(
            &Install {
                release: "metallb",
                chart: Chart::Repo {
                    url: "https://example.org/charts",
                    name: "metallb",
                },
                version: Some("0.16.1"),
                namespace: "metallb-system",
                context: "k3d-c",
                takeover: Takeover::default(),
            },
            Some("deployed"),
        );
        assert_eq!(
            args.join(" "),
            "upgrade --install metallb metallb --repo https://example.org/charts --version 0.16.1 \
             --namespace metallb-system --create-namespace --kube-context k3d-c \
             --wait --rollback-on-failure --timeout 5m"
        );
        let args = upgrade_args(
            &Install {
                release: "base",
                chart: Chart::Dir(Path::new("/tmp/chart")),
                version: None,
                namespace: "ns",
                context: "k3d-c",
                takeover: Takeover::default(),
            },
            None,
        );
        assert!(
            args.join(" ")
                .starts_with("upgrade --install base /tmp/chart --namespace ns ")
        );
    }

    #[test]
    fn taking_ownership_adopts_and_never_rolls_back() {
        let install = |take_ownership, force_conflicts| {
            let takeover = Takeover {
                take_ownership,
                force_conflicts,
            };
            upgrade_args(
                &Install {
                    release: "r",
                    chart: Chart::Dir(Path::new("/tmp/chart")),
                    version: None,
                    namespace: "ns",
                    context: "c",
                    takeover,
                },
                Some("deployed"),
            )
            .join(" ")
        };
        let tail = |args: String| args.split_once("--kube-context c ").unwrap().1.to_owned();
        assert_eq!(
            tail(install(true, false)),
            "--wait --take-ownership --timeout 5m"
        );
        assert_eq!(
            tail(install(false, true)),
            "--wait --rollback-on-failure --server-side true --force-conflicts --timeout 5m"
        );
        assert_eq!(
            tail(install(true, true)),
            "--wait --take-ownership --server-side true --force-conflicts --timeout 5m"
        );
    }

    #[test]
    fn a_release_left_failed_is_never_rolled_back() {
        let plain = Takeover::default();
        assert!(rolls_back(plain, None));
        assert!(rolls_back(plain, Some("deployed")));
        assert!(!rolls_back(plain, Some("failed")));
        let taking = Takeover {
            take_ownership: true,
            force_conflicts: false,
        };
        assert!(!rolls_back(taking, None));
        assert!(!rolls_back(taking, Some("failed")));
    }

    #[test]
    fn a_manifests_chart_prints_its_manifests_untemplated() {
        let dir = tempfile::tempdir().unwrap();
        write_manifests_chart(dir.path(), "base", "1.2.3", "a: '{{ not a template }}'\n").unwrap();
        let read = |p: &str| std::fs::read_to_string(dir.path().join(p)).unwrap();
        assert!(
            read("Chart.yaml").contains("name: base"),
            "{}",
            read("Chart.yaml")
        );
        assert!(
            read("Chart.yaml").contains("version: \"1.2.3\"\nappVersion: \"1.2.3\""),
            "{}",
            read("Chart.yaml")
        );
        assert_eq!(read("manifests.yaml"), "a: '{{ not a template }}'\n");
        assert_eq!(read("templates/manifests.yaml"), MANIFESTS_TEMPLATE);
    }

    #[test]
    fn diffs_against_the_live_cluster_on_the_given_context() {
        let diff = |options| {
            diff_args(&Diff {
                release: "r",
                chart: Path::new("/tmp/chart"),
                namespace: "ns",
                context: "admin@c",
                options,
            })
            .join(" ")
        };
        let base = "diff upgrade r /tmp/chart --namespace ns --kube-context admin@c \
                    --allow-unreleased --three-way-merge --normalize-manifests";
        assert_eq!(diff(DiffOptions::default()), base);
        let tail = |options| diff(options).strip_prefix(base).unwrap().to_owned();
        let options = |take_ownership, show_secrets| DiffOptions {
            take_ownership,
            show_secrets,
        };
        assert_eq!(tail(options(true, false)), " --take-ownership");
        assert_eq!(tail(options(false, true)), " --show-secrets");
        assert_eq!(
            tail(options(true, true)),
            " --take-ownership --show-secrets"
        );
    }

    #[test]
    fn knows_when_the_diff_plugin_is_missing() {
        assert!(diff_plugin_missing(
            "Error: unknown command \"diff\" for \"helm\"\nRun 'helm --help' for usage.\n"
        ));
        assert!(!diff_plugin_missing(
            "Error: identified at least one change, exiting with non-zero exit code\n"
        ));
        assert!(!diff_plugin_missing(
            "Error: unknown command \"nope\" for \"helm\"\n"
        ));
        assert!(
            DIFF_PLUGIN_MISSING
                .ends_with("`helm plugin install https://github.com/databus23/helm-diff`")
        );
    }

    #[test]
    fn templates_a_chart_for_the_clusters_capabilities() {
        let chart = crate::config::build::HelmChart {
            chart: "metallb".to_owned(),
            repo: Some("https://example.org/charts".to_owned()),
            version: "0.16.1".to_owned(),
            values: None,
        };
        let capabilities = Capabilities {
            kube_version: "v1.35.5+k3s1".to_owned(),
            api_versions: vec!["v1".to_owned(), "monitoring.coreos.com/v1".to_owned()],
        };
        assert_eq!(
            template_args(
                &chart,
                "r",
                "ns",
                Some(Path::new("/v.yaml")),
                Some(&capabilities)
            )
            .join(" "),
            "template r metallb --repo https://example.org/charts --version 0.16.1 --namespace ns --include-crds \
             --values /v.yaml --kube-version v1.35.5+k3s1 --api-versions v1 --api-versions monitoring.coreos.com/v1"
        );
        let oci = crate::config::build::HelmChart {
            chart: "oci://docker.io/x/y".to_owned(),
            repo: None,
            ..chart
        };
        assert_eq!(
            template_args(&oci, "r", "ns", None, None).join(" "),
            "template r oci://docker.io/x/y --version 0.16.1 --namespace ns --include-crds"
        );
    }

    #[test]
    fn strips_helms_framing() {
        assert_eq!(
            helm_message("Error: boom\nUse --debug flag to render out invalid YAML\n"),
            "boom"
        );
        assert_eq!(
            helm_message("Error: boom\n\nError: plugin \"diff\" exited with error\n"),
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
