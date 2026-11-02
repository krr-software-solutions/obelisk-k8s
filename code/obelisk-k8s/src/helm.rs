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
        // Helm's own framing: an `Error: ` prefix, and a hint about a flag we don't pass on.
        let message: Vec<&str> = stderr
            .trim()
            .trim_start_matches("Error: ")
            .lines()
            .filter(|l| !l.starts_with("Use --debug flag"))
            .collect();
        return Err(ObeliskK8sError::Helm(format!(
            "{}: helm couldn't render it: {}",
            shown.display(),
            message.join("\n").trim()
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

#[cfg(test)]
mod tests {
    use super::*;

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
