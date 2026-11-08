//! `obelisk-k8s export --in-values-path a.b.c --out-values-path d.e.f [--format yaml|json]`: hands
//! a subtree of the values to something that wants a file. Meant as a `command` values source,
//! which gets the values (as YAML) on stdin:
//!
//! ```yaml
//! - { type: command, command: [obelisk-k8s, export, --in-values-path, app.config, --out-values-path, files.config] }
//! ```
//!
//! It writes what's at `app.config` to a file (YAML, or JSON). If the values already have a path at
//! `files.config` (a string), that's the file (from where it runs: a `command` source's is the
//! repo root; its directories made, and it's overwritten if it's there), and it prints nothing to
//! merge (`{}`), so the values don't change run to run. Otherwise it's a new file in the temp dir,
//! and it prints `files: { config: <its path> }`, which the phase merges into the values: later
//! phases (and manifests) see the path. Paths are dot-separated keys (no key may hold a `.`). A new
//! file is mode 0600, and kept: nothing deletes it (a temp file's name is random, so its path
//! differs every run). With SSM refs resolved (`apply`, `template --resolve`), a subtree holding
//! secrets would be written out in plain text: it's meant for secret-free input, like
//! `obeliskd batch alloc`'s.

use std::io::Write as _;
use std::path::Path;

use serde_json::{Map, Value};

use crate::config::key_path::KeyPath;
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

const IN_FLAG: &str = "--in-values-path";
const OUT_FLAG: &str = "--out-values-path";

/// Exports what's at `in_path` in `input` (values, as YAML), written as `format`, to the file
/// named at `out_path` in `input` (from the current directory) if there's one, else to a new file in
/// `temp_dir`; returns the values to merge (as YAML): none for a named file, else `out_path` set to
/// the new file's path.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if a path is empty or has an empty key, `out_path` is under
/// `obelisk:`, `input` isn't one mapping, nothing is at `in_path`, or what's at `out_path` isn't a
/// string (or `null`); [`ObeliskK8sError::Yaml`] if `input` isn't YAML; [`ObeliskK8sError::Io`] if
/// the file can't be written.
pub fn export(
    input: &str,
    in_path: &str,
    out_path: &str,
    format: ExportFormat,
    temp_dir: &Path,
) -> Result<String, ObeliskK8sError> {
    let in_keys = key_path(in_path, IN_FLAG)?;
    let out_keys = key_path(out_path, OUT_FLAG)?;
    if out_keys.under_obelisk() {
        return Err(ObeliskK8sError::Usage(format!(
            "{OUT_FLAG} `{out_path}` is under `obelisk:`, which is the tool's: pick another key"
        )));
    }
    let values = values(input)?;
    let usage = |flag: &str, path: &str, message: String| {
        ObeliskK8sError::Usage(format!("{flag} `{path}`: {message}"))
    };
    let value = in_keys
        .get(&values)
        .map_err(|message| usage(IN_FLAG, in_path, message))?;
    let named = match out_keys
        .find(&values)
        .map_err(|message| usage(OUT_FLAG, out_path, message))?
    {
        None | Some(Value::Null) => None,
        Some(Value::String(file)) if file.is_empty() => {
            return Err(usage(
                OUT_FLAG,
                out_path,
                "is an empty string, not a file's path".to_owned(),
            ));
        }
        Some(Value::String(file)) => Some(file.as_str()),
        Some(other) => {
            return Err(usage(
                OUT_FLAG,
                out_path,
                format!("is set, but to {}, not a file's path", describe(other)),
            ));
        }
    };
    let text = text(value, in_path, format)?;

    if let Some(file) = named {
        write_named(Path::new(file), &text)?;
        return yaml::to_string(&Map::new(), Path::new(out_path));
    }
    let path = write_temp(temp_dir, &text, format)?;
    yaml::to_string(&out_keys.wrap(Value::String(path)), Path::new(out_path))
}

/// A path flag's keys.
fn key_path(path: &str, flag: &str) -> Result<KeyPath, ObeliskK8sError> {
    KeyPath::parse(path).map_err(|message| ObeliskK8sError::Usage(format!("{flag} {message}")))
}

/// The values on stdin: one mapping (nothing is an empty one).
fn values(input: &str) -> Result<Map<String, Value>, ObeliskK8sError> {
    let mut documents = yaml::parse_values_stream(input, Path::new("stdin"))?;
    if documents.len() > 1 {
        return Err(ObeliskK8sError::Usage(format!(
            "the values on stdin are {} documents, not one mapping",
            documents.len()
        )));
    }
    match documents.pop().unwrap_or(Value::Null) {
        Value::Object(values) => Ok(values),
        Value::Null => Ok(Map::new()),
        other => Err(ObeliskK8sError::Usage(format!(
            "the values on stdin are {}, not a mapping",
            describe(&other)
        ))),
    }
}

/// e.g. "a list".
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a mapping",
    }
}

/// `value` (from `in_path`), written as `format`.
fn text(value: &Value, in_path: &str, format: ExportFormat) -> Result<String, ObeliskK8sError> {
    match format {
        ExportFormat::Yaml => yaml::to_string(value, Path::new(in_path)),
        ExportFormat::Json => {
            let mut text = serde_json::to_string_pretty(value).map_err(|error| {
                ObeliskK8sError::Usage(format!("`{in_path}` can't be written as JSON: {error}"))
            })?;
            text.push('\n');
            Ok(text)
        }
    }
}

/// Writes `text` to `file`: its directories made, created mode 0600 (or overwritten, its mode
/// kept).
fn write_named(file: &Path, text: &str) -> Result<(), ObeliskK8sError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let io = |source| ObeliskK8sError::Io {
        path: file.to_owned(),
        source,
    };
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io)
}

/// Writes `text` to a new file (mode 0600, kept) in `dir`; returns its path.
fn write_temp(dir: &Path, text: &str, format: ExportFormat) -> Result<String, ObeliskK8sError> {
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
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const VALUES: &str = "obelisk:\n  unit: app\napp:\n  config:\n    port: 8080\n    name: \"0755\"\n    list: [a, b]\n";

    fn parsed(out: &str) -> Value {
        yaml::parse_values_stream(out, Path::new("x"))
            .unwrap()
            .remove(0)
    }

    fn read(out: &str) -> (Value, String) {
        let value = parsed(out);
        let path = value["files"]["config"].as_str().unwrap().to_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        (value, text)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_the_subtree_to_a_temp_file_and_points_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let export = |format| export(VALUES, "app.config", "files.config", format, dir.path());
        let out = export(ExportFormat::Yaml).unwrap();
        let (value, text) = read(&out);
        let path = value["files"]["config"].as_str().unwrap();
        assert!(
            path.starts_with(dir.path().to_str().unwrap())
                && Path::new(path).extension() == Some("yaml".as_ref()),
            "{path}"
        );
        assert_eq!(text, "port: 8080\nname: \"0755\"\nlist:\n- a\n- b\n");
        assert_eq!(mode(Path::new(path)), 0o600);

        let (value, text) = read(&export(ExportFormat::Json).unwrap());
        assert_eq!(
            Path::new(value["files"]["config"].as_str().unwrap()).extension(),
            Some("json".as_ref())
        );
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            serde_json::json!({ "port": 8080, "name": "0755", "list": ["a", "b"] })
        );

        // A scalar, at the top; and a `null` at the out path is as good as nothing.
        let input = format!("{VALUES}f: null\n");
        let out = super::export(&input, "obelisk.unit", "f", ExportFormat::Yaml, dir.path());
        let text = std::fs::read_to_string(parsed(&out.unwrap())["f"].as_str().unwrap()).unwrap();
        assert_eq!(text, "app\n");
    }

    #[test]
    fn writes_to_the_file_the_values_name() {
        let dir = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let file = dir.path().join("made/alloc.yaml");
        let input = format!("{VALUES}files:\n  config: {}\n", file.display());
        let out = export(
            &input,
            "app.config",
            "files.config",
            ExportFormat::Yaml,
            temp.path(),
        );
        // Nothing to merge: the values already have it.
        assert_eq!(out.unwrap(), "{}\n");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "port: 8080\nname: \"0755\"\nlist:\n- a\n- b\n"
        );
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);

        // Again: overwritten, its mode kept.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        export(
            &input,
            "obelisk.unit",
            "files.config",
            ExportFormat::Yaml,
            temp.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "app\n");
        assert_eq!(mode(&file), 0o644);
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
            "--in-values-path `app.nope`: no `nope` under `app`"
        );
        assert_eq!(
            error(VALUES, "nope", "f"),
            "--in-values-path `nope`: no `nope` in the values"
        );
        assert_eq!(
            error(VALUES, "obelisk.unit.x", "f"),
            "--in-values-path `obelisk.unit.x`: `obelisk.unit` isn't a mapping"
        );
        assert_eq!(
            error("", "a", "f"),
            "--in-values-path `a`: no `a` in the values"
        );
        assert_eq!(
            error("- a\n", "a", "f"),
            "the values on stdin are a list, not a mapping"
        );
        assert_eq!(
            error(VALUES, "app..config", "f"),
            "--in-values-path `app..config` has an empty key: write `a.b.c`"
        );
        assert_eq!(
            error(VALUES, "app", ""),
            "--out-values-path `` has an empty key: write `a.b.c`"
        );
        assert_eq!(
            error(VALUES, "app", "obelisk.release.version"),
            "--out-values-path `obelisk.release.version` is under `obelisk:`, which is the tool's: pick another key"
        );
        assert_eq!(
            error(VALUES, "app", "app.config"),
            "--out-values-path `app.config`: is set, but to a mapping, not a file's path"
        );
        assert_eq!(
            error(&format!("{VALUES}f: \"\"\n"), "app", "f"),
            "--out-values-path `f`: is an empty string, not a file's path"
        );
        assert_eq!(
            error(VALUES, "app", "app.config.port.x"),
            "--out-values-path `app.config.port.x`: `app.config.port` isn't a mapping"
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
