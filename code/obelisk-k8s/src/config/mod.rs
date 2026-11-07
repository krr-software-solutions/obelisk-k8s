//! Config files: a repo's own (`repo`), and the machine-wide one (`global`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ObeliskK8sError;
use crate::error::LineCol;
use crate::yaml::Spanned;

pub mod apply;
pub mod build;
pub mod diff;
pub mod ephemeral;
pub mod global;
pub mod images;
pub mod manifests;
pub mod paths;
pub mod phases;
pub mod pull_secret;
pub mod push;
mod render;
pub mod repo;
mod run;
pub mod values;

/// One `<kind>/<dir>/<file>` to load.
pub(crate) struct Entry {
    /// The real path to the entry's directory.
    pub(crate) dir: PathBuf,
    /// The real path to the file.
    pub(crate) path: PathBuf,
    /// The path to show the user, relative to the repo root.
    pub(crate) file: PathBuf,
    /// `None` if the directory name isn't UTF-8.
    pub(crate) dir_name: Option<String>,
}

/// Every `<dir>/<kind>/<entry>/<file_name>`, in path order. Hidden entries are skipped; a stray file,
/// or a directory without `file_name`, is skipped with a warning.
pub(crate) fn read_entries(
    dir: &Path,
    shown: &Path,
    kind: &str,
    file_name: &str,
    warnings: &mut Vec<String>,
) -> Result<Vec<Entry>, ObeliskK8sError> {
    let kind_dir = dir.join(kind);
    if !kind_dir.exists() {
        return Ok(Vec::new());
    }
    let shown_kind_dir = shown.join(kind);
    let io_error = |source| ObeliskK8sError::Io {
        path: shown_kind_dir.clone(),
        source,
    };

    let mut dir_entries = std::fs::read_dir(&kind_dir)
        .map_err(io_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error)?;
    dir_entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut entries = Vec::new();
    for dir_entry in dir_entries {
        let os_name = dir_entry.file_name();
        if os_name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let shown_entry = shown_kind_dir.join(&os_name);
        if !dir_entry.path().is_dir() {
            warnings.push(format!(
                "{}: ignored: not a directory (expected {kind}/<name>/{file_name})",
                shown_entry.display()
            ));
            continue;
        }
        let path = dir_entry.path().join(file_name);
        if !path.is_file() {
            warnings.push(format!(
                "{}: ignored: no `{file_name}` in this directory",
                shown_entry.display()
            ));
            continue;
        }
        entries.push(Entry {
            dir: dir_entry.path(),
            path,
            file: shown_entry.join(file_name),
            dir_name: os_name.to_str().map(str::to_owned),
        });
    }
    Ok(entries)
}

/// `entry`'s name: `name` if it's given, else its directory's.
pub(crate) fn name_or_dir(
    entry: &Entry,
    name: Option<Spanned<String>>,
) -> Result<String, ObeliskK8sError> {
    if let Some(name) = name {
        check_name(&entry.file, &name)?;
        return Ok(name.value);
    }
    dir_name(entry, "rename it or set `name`")
}

/// `entry`'s directory's name, which must be a valid name; `fix` says what to do if it isn't.
pub(crate) fn dir_name(entry: &Entry, fix: &str) -> Result<String, ObeliskK8sError> {
    let Some(dir_name) = &entry.dir_name else {
        return Err(config_error(
            &entry.file,
            None,
            format!("the directory name isn't UTF-8: {fix}"),
        ));
    };
    if !is_valid_name(dir_name) {
        return Err(config_error(
            &entry.file,
            None,
            format!("the directory name `{dir_name}` isn't a valid name ({NAME_RULE}): {fix}"),
        ));
    }
    Ok(dir_name.clone())
}

pub(crate) const NAME_RULE: &str =
    "at most 63 lowercase letters, digits and `-`, starting and ending with a letter or digit";

/// A DNS label: names end up in paths and Helm release names.
pub(crate) fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    bytes.len() <= 63
        && bytes.iter().all(|b| alnum(b) || *b == b'-')
        && bytes.first().is_some_and(alnum)
        && bytes.last().is_some_and(alnum)
}

pub(crate) fn check_name(file: &Path, name: &Spanned<String>) -> Result<(), ObeliskK8sError> {
    if is_valid_name(&name.value) {
        Ok(())
    } else {
        Err(config_error(
            file,
            Some(name.at),
            format!("`{}` isn't a valid name ({NAME_RULE})", name.value),
        ))
    }
}

/// Errors on the first name used twice, at the later file (in path order).
pub(crate) fn check_unique<'a>(
    items: impl Iterator<Item = (&'a str, &'a Path)>,
    kind: &str,
) -> Result<(), ObeliskK8sError> {
    let mut seen = HashMap::new();
    for (name, file) in items {
        if let Some(first) = seen.insert(name, file) {
            return Err(config_error(
                file,
                None,
                format!(
                    "{kind} name `{name}` is already used by {}",
                    first.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn config_error(
    file: &Path,
    at: Option<LineCol>,
    message: impl Into<String>,
) -> ObeliskK8sError {
    ObeliskK8sError::Config {
        file: file.to_owned(),
        at,
        message: message.into(),
    }
}

/// Writing `.obelisk-k8s/` fixtures, and the `helm` gate, for tests across the config modules.
#[cfg(test)]
pub(crate) mod test_support {
    use super::repo::REPO_CONFIG_DIR;

    /// A temp repo root with `files` (paths relative to `.obelisk-k8s/`) written.
    pub(crate) fn write_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(REPO_CONFIG_DIR).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// Writes an executable script at `path` (making its directory), from a child process: a file
    /// this process wrote can't run (`ETXTBSY`, "Text file busy") while a child another test is
    /// forking still holds the write descriptor it inherited, until that child execs. Closing it
    /// first, or renaming it into place, doesn't help (the child's copy is of the same file); a file
    /// only ever open for writing in a process that has exited can't be held that way.
    pub(crate) fn write_script(path: &std::path::Path, text: &str) {
        use std::io::Write as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "writing {}",
            path.display()
        );
    }

    /// A target on cluster `c` whose units' release is the unit's name, in a namespace named after
    /// the target.
    pub(crate) const TARGET: &str = "target:\n  cluster: c\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\" }\n";

    /// A global config with clusters `c` (the one test targets deploy to; no obeliskd) and `obd` (on
    /// obeliskd supernet `main`, subnet `sub`).
    pub(crate) fn global() -> crate::config::global::GlobalConfig {
        use crate::config::global::{Cluster, GlobalConfig, K8s, Location, Obeliskd};
        let cluster = |name: &str, obeliskd: Option<Obeliskd>| Cluster {
            name: name.to_owned(),
            file: format!("clusters/{name}/cluster.yaml").into(),
            dir: format!("/home/me/.obelisk-k8s/clusters/{name}").into(),
            k8s: K8s::K3d,
            location: Location::Local,
            managed: true,
            obeliskd,
            kubectl_cluster: None,
            kubectl_context: None,
            docker: None,
        };
        GlobalConfig {
            root: "~/.obelisk-k8s".into(),
            dir: "/home/me/.obelisk-k8s".into(),
            repos: Vec::new(),
            clusters: vec![
                cluster("c", None),
                cluster(
                    "obd",
                    Some(Obeliskd {
                        supernet: "main".to_owned(),
                        subnet: "sub".to_owned(),
                    }),
                ),
            ],
            warnings: Vec::new(),
        }
    }

    /// For tests that run `helm`: without it they fail, unless `OBELISK_TEST_SKIP_HELM=1` says to skip
    /// them (then this is `false`, and the test returns early).
    pub(crate) fn helm_installed() -> bool {
        if std::process::Command::new("helm")
            .arg("version")
            .output()
            .is_ok()
        {
            return true;
        }
        assert!(
            std::env::var_os("OBELISK_TEST_SKIP_HELM").is_some_and(|v| v == "1"),
            "these tests need `helm` on the PATH (or OBELISK_TEST_SKIP_HELM=1 to skip them)"
        );
        false
    }
}
