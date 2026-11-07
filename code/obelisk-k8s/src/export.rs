//! `obelisk-k8s export --in-path a.b.c --out-path d.e.f [--format yaml|json]`: hands a subtree of
//! the values to something that wants a file. Meant as a `command` values source, which gets the
//! values (as YAML) on stdin:
//!
//! ```yaml
//! - { type: command, command: [obelisk-k8s, export, --in-path, app.config, --out-path, files.config] }
//! ```
//!
//! It writes what's at `app.config` to a new file in the temp dir (YAML, or JSON), then prints
//! `files: { config: <its path> }`, which the phase merges into the values: later phases (and
//! manifests) see the path. Paths are dot-separated keys (no key may hold a `.`). The file is kept
//! (mode 0600): nothing deletes it, and its name is random, so the path differs every run. With SSM
//! refs resolved (`apply`, `template --resolve`), a subtree holding secrets would be written out in
//! plain text: it's meant for secret-free input, like `obeliskd batch alloc`'s.

use std::io::Write as _;
use std::path::Path;

use serde_json::{Map, Value};

use crate::{ObeliskK8sError, yaml};

/// What the exported file is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    #[default]
    Yaml,
    Json,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Yaml => "yaml",
            Self::Json => "json",
        }
    }
}

/// Exports what's at `in_path` in `input` (values, as YAML) to a new file in `dir`, written as
/// `format`; returns the values that point at it (`out_path` set to its path), as YAML.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if a path is empty or has an empty key, `input` isn't one mapping,
/// or nothing is at `in_path`; [`ObeliskK8sError::Yaml`] if `input` isn't YAML;
/// [`ObeliskK8sError::Io`] if the file can't be written.
pub fn export(
    input: &str,
    in_path: &str,
    out_path: &str,
    format: ExportFormat,
    dir: &Path,
) -> Result<String, ObeliskK8sError> {
    let in_keys = keys(in_path, "--in-path")?;
    let out_keys = keys(out_path, "--out-path")?;
    if out_keys[0] == "obelisk" {
        return Err(ObeliskK8sError::Usage(format!(
            "--out-path `{out_path}` is under `obelisk:`, which is the tool's: pick another key"
        )));
    }
    let stdin = Path::new("stdin");
    let mut documents = yaml::parse_values_stream(input, stdin)?;
    if documents.len() > 1 {
        return Err(ObeliskK8sError::Usage(format!(
            "the values on stdin are {} documents, not one mapping",
            documents.len()
        )));
    }
    let values = documents.pop().unwrap_or(Value::Null);
    let value = lookup(&values, &in_keys)?;

    let text = match format {
        ExportFormat::Yaml => yaml::to_string(value, Path::new(in_path))?,
        ExportFormat::Json => {
            let mut text = serde_json::to_string_pretty(value).map_err(|error| {
                ObeliskK8sError::Usage(format!("`{in_path}` can't be written as JSON: {error}"))
            })?;
            text.push('\n');
            text
        }
    };
    let io = |source| ObeliskK8sError::Io {
        path: dir.to_owned(),
        source,
    };
    let mut file = tempfile::Builder::new()
        .prefix("obelisk-k8s-export-")
        .suffix(&format!(".{}", format.extension()))
        .tempfile_in(dir)
        .map_err(io)?;
    // Its path goes into the values: refuse one that isn't UTF-8 before keeping anything.
    let path = file
        .path()
        .to_str()
        .ok_or_else(|| ObeliskK8sError::Usage(format!("{} isn't UTF-8", file.path().display())))?
        .to_owned();
    file.write_all(text.as_bytes()).map_err(io)?;
    file.keep().map_err(|error| io(error.error))?;

    let mut out = Value::String(path);
    for key in out_keys.iter().rev() {
        let mut map = Map::new();
        map.insert((*key).to_owned(), out);
        out = Value::Object(map);
    }
    yaml::to_string(&out, Path::new(out_path))
}

/// A dot-separated path's keys (`flag` names it in errors).
fn keys<'p>(path: &'p str, flag: &str) -> Result<Vec<&'p str>, ObeliskK8sError> {
    let keys: Vec<&str> = path.split('.').collect();
    if keys.iter().any(|k| k.is_empty()) {
        return Err(ObeliskK8sError::Usage(format!(
            "{flag} `{path}` has an empty key: write `a.b.c`"
        )));
    }
    Ok(keys)
}

/// What's at `keys` in `values`.
fn lookup<'v>(values: &'v Value, keys: &[&str]) -> Result<&'v Value, ObeliskK8sError> {
    let mut at = values;
    for (i, key) in keys.iter().enumerate() {
        let here = keys[..i].join(".");
        at = match at {
            Value::Object(map) => map.get(*key).ok_or_else(|| {
                let under = if here.is_empty() {
                    "in the values".to_owned()
                } else {
                    format!("under `{here}`")
                };
                ObeliskK8sError::Usage(format!(
                    "--in-path `{}`: no `{key}` {under}",
                    keys.join(".")
                ))
            })?,
            _ => {
                return Err(ObeliskK8sError::Usage(format!(
                    "--in-path `{}`: {}",
                    keys.join("."),
                    if here.is_empty() {
                        "the values aren't a mapping".to_owned()
                    } else {
                        format!("`{here}` isn't a mapping")
                    }
                )));
            }
        };
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUES: &str = "obelisk:\n  unit: app\napp:\n  config:\n    port: 8080\n    name: \"0755\"\n    list: [a, b]\n";

    fn read(out: &str) -> (Value, String) {
        let value: Value = yaml::parse_values_stream(out, Path::new("x"))
            .unwrap()
            .remove(0);
        let path = value["files"]["config"].as_str().unwrap().to_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        (value, text)
    }

    #[test]
    fn writes_the_subtree_and_points_at_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let out = export(
            VALUES,
            "app.config",
            "files.config",
            ExportFormat::Yaml,
            dir.path(),
        )
        .unwrap();
        let (value, text) = read(&out);
        let path = value["files"]["config"].as_str().unwrap();
        assert!(
            path.starts_with(dir.path().to_str().unwrap())
                && Path::new(path).extension() == Some("yaml".as_ref()),
            "{path}"
        );
        assert_eq!(text, "port: 8080\nname: \"0755\"\nlist:\n- a\n- b\n");
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let out = export(
            VALUES,
            "app.config",
            "files.config",
            ExportFormat::Json,
            dir.path(),
        )
        .unwrap();
        let (value, text) = read(&out);
        assert_eq!(
            Path::new(value["files"]["config"].as_str().unwrap()).extension(),
            Some("json".as_ref())
        );
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            serde_json::json!({ "port": 8080, "name": "0755", "list": ["a", "b"] })
        );

        // A scalar, at the top.
        let out = export(VALUES, "obelisk.unit", "f", ExportFormat::Yaml, dir.path()).unwrap();
        let value: Value = yaml::parse_values_stream(&out, Path::new("x"))
            .unwrap()
            .remove(0);
        let text = std::fs::read_to_string(value["f"].as_str().unwrap()).unwrap();
        assert_eq!(text, "app\n");
    }

    #[test]
    fn refuses_bad_paths_and_input() {
        let dir = tempfile::tempdir().unwrap();
        let error = |input: &str, in_path: &str, out_path: &str| {
            export(input, in_path, out_path, ExportFormat::Yaml, dir.path())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error(VALUES, "app.nope", "f"),
            "--in-path `app.nope`: no `nope` under `app`"
        );
        assert_eq!(
            error(VALUES, "nope", "f"),
            "--in-path `nope`: no `nope` in the values"
        );
        assert_eq!(
            error(VALUES, "obelisk.unit.x", "f"),
            "--in-path `obelisk.unit.x`: `obelisk.unit` isn't a mapping"
        );
        assert_eq!(
            error("", "a", "f"),
            "--in-path `a`: the values aren't a mapping"
        );
        assert_eq!(
            error(VALUES, "app..config", "f"),
            "--in-path `app..config` has an empty key: write `a.b.c`"
        );
        assert_eq!(
            error(VALUES, "app", ""),
            "--out-path `` has an empty key: write `a.b.c`"
        );
        assert_eq!(
            error(VALUES, "app", "obelisk.release.version"),
            "--out-path `obelisk.release.version` is under `obelisk:`, which is the tool's: pick another key"
        );
        assert_eq!(
            error("a: 1\n---\nb: 2\n", "a", "f"),
            "the values on stdin are 2 documents, not one mapping"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "nothing written"
        );
    }
}
