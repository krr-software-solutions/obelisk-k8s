//! Changing a cluster's `docker.yaml` (see `config::global::docker`): `config global docker show`,
//! `push set` / `unset-credentials`, `pull add` / `set` / `remove`, and `remove`.
//!
//! The file is obelisk-k8s's own and small, so a change rewrites it whole, from what was loaded
//! (comments in it don't survive; a command that changes nothing doesn't write). It's written
//! through a temp file and a rename (where it's a symlink, the file it points to), mode 0600 when
//! it's new, else its own mode, tightened to the owner's when it then holds a plain-text password.
//! Then the global config is loaded again, and the old file put back if that fails. Credentials are
//! taken as given: SSM refs, or plain text. A file that won't load can still be `remove`d. There's
//! no locking: two edits at once may lose one.

use std::fmt;
use std::fs::Permissions;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Change, undo_failed, usage};
use crate::ObeliskK8sError;
use crate::config::global::docker::{
    Auth, Credentials, DOCKER_FILE, Docker, Push, check_host, check_prefix,
};
use crate::config::global::{Cluster, GlobalConfig, tilde};
use crate::ssm;

/// A username and password, given together.
#[derive(Debug, Clone)]
pub struct NewCredentials {
    pub username: String,
    pub password: String,
}

/// What `push set` changes: the prefix and/or the credentials (each left as is if `None`).
#[derive(Debug, Clone, Default)]
pub struct PushChange {
    pub prefix: Option<String>,
    pub credentials: Option<NewCredentials>,
}

/// What `pull set` changes for a host: the username and/or password (each left as is if `None`).
#[derive(Debug, Clone, Default)]
pub struct AuthChange {
    pub username: Option<String>,
    pub password: Option<String>,
}

/// A cluster's `docker.yaml` as `config global docker show` prints it: passwords masked unless
/// they're just one SSM ref.
#[derive(Debug)]
pub struct DockerShow {
    pub cluster: String,
    pub file: PathBuf,
    pub docker: Option<Docker>,
    /// Whether it's a local k3d cluster (which imports images without one).
    pub local_k3d: bool,
    /// The global config's.
    pub warnings: Vec<String>,
}

/// Cluster `cluster`'s `docker.yaml`, for showing.
///
/// # Errors
///
/// Whatever loading the global config fails with; [`ObeliskK8sError::Usage`] for an unknown cluster.
pub fn show(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
) -> Result<DockerShow, ObeliskK8sError> {
    let config = GlobalConfig::load(root, home)?;
    let cluster = config.cluster(cluster)?;
    Ok(DockerShow {
        cluster: cluster.name.clone(),
        file: shown_file(cluster),
        docker: cluster.docker.clone(),
        local_k3d: cluster.is_local_k3d(),
        warnings: config.warnings.clone(),
    })
}

/// Sets cluster `cluster`'s push prefix and/or credentials, creating its `docker.yaml` (which then
/// needs a prefix).
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster, nothing to change, or no prefix for a new
/// file; whatever loading the global config fails with, before or after (after: the file is put
/// back).
pub fn push_set(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
    change: PushChange,
) -> Result<Change, ObeliskK8sError> {
    if change.prefix.is_none() && change.credentials.is_none() {
        return Err(usage(
            "nothing to set: give `--prefix`, or `--username` and `--password`".to_owned(),
        ));
    }
    if let Some(prefix) = &change.prefix {
        check_prefix(prefix).map_err(|message| usage(format!("`--prefix`: {message}")))?;
    }
    if let Some(credentials) = &change.credentials {
        check_given(&credentials.username, &credentials.password)?;
    }
    edit(root, home, cluster, |file, docker| {
        let mut push = match docker {
            Some(docker) => docker.push.clone(),
            None => Push {
                prefix: change.prefix.clone().ok_or_else(|| {
                    usage(format!(
                        "{} doesn't exist yet: give `--prefix` to create it",
                        file.display()
                    ))
                })?,
                credentials: None,
            },
        };
        let mut set = Vec::new();
        if let Some(prefix) = change.prefix {
            set.push(format!("the push prefix `{prefix}`"));
            push.prefix = prefix;
        }
        if let Some(credentials) = change.credentials {
            set.push("the push credentials".to_owned());
            push.credentials = Some(Credentials {
                username: credentials.username,
                password: credentials.password,
            });
        }
        let message = format!("set {}", set.join(" and "));
        let pull_auths = docker.map(|d| d.pull_auths.clone()).unwrap_or_default();
        Ok((
            Some(Docker {
                file: file.to_owned(),
                push,
                pull_auths,
            }),
            message,
        ))
    })
}

/// Drops cluster `cluster`'s push credentials: pushes then use the user's own `docker login`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster or one without a `docker.yaml`; as for
/// [`push_set`].
pub fn push_unset_credentials(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
) -> Result<Change, ObeliskK8sError> {
    edit(root, home, cluster, |file, docker| {
        let mut docker = existing(file, docker)?;
        if docker.push.credentials.take().is_none() {
            return Ok((Some(docker), "no push credentials".to_owned()));
        }
        Ok((Some(docker), "unset the push credentials".to_owned()))
    })
}

/// Adds pull credentials for `host` to cluster `cluster`'s `docker.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster, one without a `docker.yaml`, or a host already
/// listed; as for [`push_set`].
pub fn pull_add(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
    host: &str,
    credentials: NewCredentials,
) -> Result<Change, ObeliskK8sError> {
    check_host(host).map_err(|message| usage(format!("`--host`: {message}")))?;
    check_given(&credentials.username, &credentials.password)?;
    edit(root, home, cluster, |file, docker| {
        let mut docker = existing(file, docker)?;
        if docker
            .pull_auths
            .iter()
            .any(|a| a.host.eq_ignore_ascii_case(host))
        {
            return Err(usage(format!(
                "`{host}` already has pull credentials in {} (`pull set` changes them)",
                file.display()
            )));
        }
        docker.pull_auths.push(Auth {
            host: host.to_owned(),
            credentials: Credentials {
                username: credentials.username,
                password: credentials.password,
            },
        });
        Ok((Some(docker), format!("added pull credentials for `{host}`")))
    })
}

/// Changes the pull credentials for `host` in cluster `cluster`'s `docker.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster, one without a `docker.yaml`, an unlisted host,
/// or nothing to change; as for [`push_set`].
pub fn pull_set(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
    host: &str,
    change: AuthChange,
) -> Result<Change, ObeliskK8sError> {
    if change.username.is_none() && change.password.is_none() {
        return Err(usage(
            "nothing to set: give `--username` and/or `--password`".to_owned(),
        ));
    }
    check_given(
        change.username.as_deref().unwrap_or("-"),
        change.password.as_deref().unwrap_or("-"),
    )?;
    edit(root, home, cluster, |file, docker| {
        let mut docker = existing(file, docker)?;
        let auth = find_auth(file, &mut docker, host)?;
        if let Some(username) = change.username {
            auth.credentials.username = username;
        }
        if let Some(password) = change.password {
            auth.credentials.password = password;
        }
        let host = auth.host.clone();
        Ok((
            Some(docker),
            format!("set the pull credentials for `{host}`"),
        ))
    })
}

/// Removes the pull credentials for `host` from cluster `cluster`'s `docker.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster, one without a `docker.yaml`, or an unlisted
/// host; as for [`push_set`].
pub fn pull_remove(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
    host: &str,
) -> Result<Change, ObeliskK8sError> {
    edit(root, home, cluster, |file, docker| {
        let mut docker = existing(file, docker)?;
        let host = find_auth(file, &mut docker, host)?.host.clone();
        docker.pull_auths.retain(|a| a.host != host);
        Ok((
            Some(docker),
            format!("removed the pull credentials for `{host}`"),
        ))
    })
}

/// Deletes cluster `cluster`'s `docker.yaml`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] for an unknown cluster or one without a `docker.yaml`; as for
/// [`push_set`].
pub fn remove(root: &Path, home: Option<&Path>, cluster: &str) -> Result<Change, ObeliskK8sError> {
    match GlobalConfig::load(root, home) {
        Ok(config) => {
            let found = config.cluster(cluster)?;
            let path = found.dir.join(DOCKER_FILE);
            // A link to nothing loads as no file: it's still there to remove.
            if found.docker.is_none() && std::fs::symlink_metadata(&path).is_ok() {
                let shown = shown_file(found);
                std::fs::remove_file(&path).map_err(|source| ObeliskK8sError::Io {
                    path: shown.clone(),
                    source,
                })?;
                return Ok(Change {
                    message: format!("{}: removed it (a link to nothing)", shown.display()),
                    warnings: config.warnings,
                });
            }
            edit(root, home, cluster, |file, docker| {
                existing(file, docker)?;
                Ok((None, "removed it".to_owned()))
            })
        }
        // A `docker.yaml` that won't load stops everything: it's still removable, if that's what
        // was wrong.
        Err(error) => {
            let Some(dir) = cluster_dir(root, cluster) else {
                return Err(error);
            };
            let path = dir.join(DOCKER_FILE);
            if std::fs::symlink_metadata(&path).is_err() {
                return Err(error);
            }
            let shown = tilde(&path, home);
            let file = File::at(&path, &shown)?;
            file.remove()?;
            let after = GlobalConfig::load(root, home).map_err(|error| file.undo(error))?;
            Ok(Change {
                message: format!("{}: removed it (it didn't load)", shown.display()),
                warnings: after.warnings,
            })
        }
    }
}

/// Cluster `name`'s directory, without loading the global config: the `clusters/<dir>/` whose
/// `cluster.yaml` names it, else (none naming itself so) `clusters/<name>/`.
fn cluster_dir(root: &Path, name: &str) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct File {
        cluster: Section,
    }
    #[derive(serde::Deserialize)]
    struct Section {
        name: Option<String>,
    }
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return None;
    }
    let clusters = root.join("clusters");
    let named = |dir: &Path| -> Option<String> {
        let path = dir.join("cluster.yaml");
        let file: File = crate::yaml::read_file(&path, &path).ok()?;
        Some(file.cluster.name.unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }))
    };
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&clusters)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.iter()
        .find(|dir| named(dir).as_deref() == Some(name))
        .cloned()
        .or_else(|| Some(clusters.join(name)).filter(|dir| dir.is_dir()))
}

/// The `docker.yaml` of `cluster`, as the user should see it.
fn shown_file(cluster: &Cluster) -> PathBuf {
    cluster.file.with_file_name(DOCKER_FILE)
}

/// `docker`, which must exist (in `file`).
fn existing(file: &Path, docker: Option<&Docker>) -> Result<Docker, ObeliskK8sError> {
    docker.cloned().ok_or_else(|| {
        usage(format!(
            "{} doesn't exist: `config global docker push set --prefix …` creates it",
            file.display()
        ))
    })
}

/// A given username and password: neither empty.
fn check_given(username: &str, password: &str) -> Result<(), ObeliskK8sError> {
    match (username.is_empty(), password.is_empty()) {
        (false, false) => Ok(()),
        (true, _) => Err(usage("`--username` is empty".to_owned())),
        (_, true) => Err(usage("`--password` is empty".to_owned())),
    }
}

/// The pull auth for `host` (any case) in `docker`.
fn find_auth<'d>(
    file: &Path,
    docker: &'d mut Docker,
    host: &str,
) -> Result<&'d mut Auth, ObeliskK8sError> {
    let listed: Vec<String> = docker
        .pull_auths
        .iter()
        .map(|a| format!("`{}`", a.host))
        .collect();
    docker
        .pull_auths
        .iter_mut()
        .find(|a| a.host.eq_ignore_ascii_case(host))
        .ok_or_else(|| {
            usage(format!(
                "no pull credentials for `{host}` in {} (there are: {})",
                file.display(),
                if listed.is_empty() {
                    "none".to_owned()
                } else {
                    listed.join(", ")
                }
            ))
        })
}

/// Loads the global config, applies `change` to cluster `cluster`'s `docker.yaml` (given as shown,
/// and as loaded, if it exists; giving back the new one, `None` to delete it, and what it did),
/// writes it, and loads the global config again (putting the old file back if that fails).
fn edit(
    root: &Path,
    home: Option<&Path>,
    cluster: &str,
    change: impl FnOnce(&Path, Option<&Docker>) -> Result<(Option<Docker>, String), ObeliskK8sError>,
) -> Result<Change, ObeliskK8sError> {
    let config = GlobalConfig::load(root, home)?;
    let cluster = config.cluster(cluster)?;
    let shown = shown_file(cluster);
    let (new, what) = change(&shown, cluster.docker.as_ref())?;
    let message = format!("{}: {what}", shown.display());
    // Nothing changed: the file (its comments, its link) stays as it is.
    if new.as_ref() == cluster.docker.as_ref() {
        return Ok(Change {
            message: format!("{}: nothing to change ({what})", shown.display()),
            warnings: config.warnings,
        });
    }
    let file = File::at(&cluster.dir.join(DOCKER_FILE), &shown)?;
    let mut warnings = Vec::new();
    match &new {
        Some(docker) => {
            let text = crate::yaml::to_string(&DockerFileOut::from(docker), &shown)?;
            let mut mode = file.original.as_ref().map_or(0o600, |(_, mode)| *mode);
            let plaintext = docker
                .push
                .credentials
                .iter()
                .chain(docker.pull_auths.iter().map(|a| &a.credentials))
                .any(|c| !ssm::is_one_ref(&c.password));
            if plaintext && mode & 0o077 != 0 {
                mode &= !0o077;
                warnings.push(format!(
                    "{}: it holds a plain-text password, so only you can read it now (mode {mode:o})",
                    shown.display()
                ));
            }
            file.write(text.as_bytes(), mode)?;
        }
        None => file.remove()?,
    }
    let after = GlobalConfig::load(root, home).map_err(|error| file.undo(error))?;
    warnings.extend(after.warnings);
    Ok(Change { message, warnings })
}

/// A `docker.yaml` being changed: where it really is (through a symlink, if it's one), and what was
/// there before.
struct File {
    /// As written in the cluster's directory.
    path: PathBuf,
    /// Where its contents are: `path`, or the file it links to.
    target: PathBuf,
    /// What `path` links to, if it's a link.
    link: Option<PathBuf>,
    /// Its contents and mode, if it existed.
    original: Option<(Vec<u8>, u32)>,
    shown: PathBuf,
}

impl File {
    fn at(path: &Path, shown: &Path) -> Result<Self, ObeliskK8sError> {
        let io = |source| ObeliskK8sError::Io {
            path: shown.to_owned(),
            source,
        };
        let link = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                Some(std::fs::read_link(path).map_err(io)?)
            }
            _ => None,
        };
        let target = match &link {
            // A link to nothing yet: writing creates what it points to.
            Some(to) => std::fs::canonicalize(path).or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    Ok(path.parent().unwrap_or(Path::new(".")).join(to))
                }
                _ => Err(io(error)),
            })?,
            None => path.to_owned(),
        };
        let original = match std::fs::read(&target) {
            Ok(bytes) => {
                let mode = std::fs::metadata(&target).map_err(io)?.permissions().mode() & 0o7777;
                Some((bytes, mode))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io(error)),
        };
        Ok(Self {
            path: path.to_owned(),
            target,
            link,
            original,
            shown: shown.to_owned(),
        })
    }

    fn io_error(&self, source: std::io::Error) -> ObeliskK8sError {
        ObeliskK8sError::Io {
            path: self.shown.clone(),
            source,
        }
    }

    /// Writes its contents (where a link points, the file it points to).
    fn write(&self, bytes: &[u8], mode: u32) -> Result<(), ObeliskK8sError> {
        replace(&self.target, bytes, mode).map_err(|e| self.io_error(e))
    }

    /// Deletes it (a link, not what it points to).
    fn remove(&self) -> Result<(), ObeliskK8sError> {
        std::fs::remove_file(&self.path).map_err(|e| self.io_error(e))
    }

    /// Puts back what was there, after `error`.
    fn undo(&self, error: ObeliskK8sError) -> ObeliskK8sError {
        let undone = match (&self.original, &self.link) {
            (Some(_), Some(link)) if std::fs::symlink_metadata(&self.path).is_err() => {
                std::os::unix::fs::symlink(link, &self.path)
            }
            (Some((bytes, mode)), _) => replace(&self.target, bytes, *mode),
            (None, _) => std::fs::remove_file(&self.path),
        };
        undo_failed(error, undone, &self.shown)
    }
}

/// Writes `bytes` to `path` through a temp file (created with `mode`) and a rename.
fn replace(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let temp = path.with_file_name(format!(".{DOCKER_FILE}.{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            // `mode` was masked by the umask: set it as asked.
            file.set_permissions(Permissions::from_mode(mode))?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

// The file, as written.

#[derive(Serialize)]
struct DockerFileOut<'a> {
    docker: DockerOut<'a>,
}

#[derive(Serialize)]
struct DockerOut<'a> {
    push: PushOut<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull: Option<PullOut<'a>>,
}

#[derive(Serialize)]
struct PushOut<'a> {
    prefix: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'a str>,
}

#[derive(Serialize)]
struct PullOut<'a> {
    auths: Vec<AuthOut<'a>>,
}

#[derive(Serialize)]
struct AuthOut<'a> {
    host: &'a str,
    username: &'a str,
    password: &'a str,
}

impl<'a> From<&'a Docker> for DockerFileOut<'a> {
    fn from(docker: &'a Docker) -> Self {
        let credentials = docker.push.credentials.as_ref();
        let auths: Vec<AuthOut<'a>> = docker
            .pull_auths
            .iter()
            .map(|a| AuthOut {
                host: &a.host,
                username: &a.credentials.username,
                password: &a.credentials.password,
            })
            .collect();
        Self {
            docker: DockerOut {
                push: PushOut {
                    prefix: &docker.push.prefix,
                    username: credentials.map(|c| c.username.as_str()),
                    password: credentials.map(|c| c.password.as_str()),
                },
                pull: (!auths.is_empty()).then_some(PullOut { auths }),
            },
        }
    }
}

/// A password as shown: as written if it's just one SSM ref, else masked.
fn masked(text: &str) -> &str {
    if ssm::is_one_ref(text) {
        text
    } else {
        "<masked>"
    }
}

impl fmt::Display for DockerShow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(docker) = &self.docker else {
            let what = if self.local_k3d {
                "its images are `k3d image import`ed instead"
            } else {
                "it has nowhere to push images: `config global docker push set --prefix …` adds one"
            };
            return writeln!(
                f,
                "cluster `{}` has no {} ({what})",
                self.cluster,
                self.file.display()
            );
        };
        writeln!(f, "{}:", self.file.display())?;
        writeln!(f, "  push: {}", docker.push.prefix)?;
        match &docker.push.credentials {
            Some(c) => writeln!(
                f,
                "    username: {}\n    password: {}",
                c.username,
                masked(&c.password)
            )?,
            None => writeln!(f, "    (no credentials: your own `docker login`)")?,
        }
        if docker.pull_auths.is_empty() {
            writeln!(f, "  pull: (no credentials)")?;
        } else {
            writeln!(f, "  pull:")?;
            for auth in &docker.pull_auths {
                writeln!(
                    f,
                    "    {}: username {}, password {}",
                    auth.host,
                    auth.credentials.username,
                    masked(&auth.credentials.password)
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A global root with clusters `k` (local k3d) and `r` (remote).
    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            (
                "k",
                "cluster:\n  k8s: k3d\n  location: local\n  managed: false\n",
            ),
            (
                "r",
                "cluster:\n  k8s: talos\n  location: remote\n  managed: false\n  kubectlContext: admin@r\n",
            ),
        ] {
            let cluster = dir.path().join("clusters").join(name);
            std::fs::create_dir_all(&cluster).unwrap();
            std::fs::write(cluster.join("cluster.yaml"), text).unwrap();
        }
        dir
    }

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("clusters/r/docker.yaml")
    }

    fn creds(username: &str, password: &str) -> NewCredentials {
        NewCredentials {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    #[test]
    fn creates_edits_and_removes_a_docker_yaml() {
        let dir = root();
        let root = dir.path();
        let set = |change| push_set(root, None, "r", change);
        assert_eq!(
            set(PushChange {
                credentials: Some(creds("u", "p")),
                ..PushChange::default()
            })
            .unwrap_err()
            .to_string(),
            format!(
                "{}/clusters/r/docker.yaml doesn't exist yet: give `--prefix` to create it",
                root.display()
            )
        );
        let change = set(PushChange {
            prefix: Some("reg.example.com/krrss".to_owned()),
            credentials: Some(creds("pusher", "${{obelisk:aws-ssm:/push/pw}}")),
        })
        .unwrap();
        assert!(
            change
                .message
                .ends_with("clusters/r/docker.yaml: set the push prefix `reg.example.com/krrss` and the push credentials"),
            "{}",
            change.message
        );
        assert_eq!(
            std::fs::metadata(file(&dir)).unwrap().permissions().mode() & 0o777,
            0o600
        );

        pull_add(
            root,
            None,
            "r",
            "reg.example.com",
            creds("puller", "hunter2"),
        )
        .unwrap();
        pull_add(
            root,
            None,
            "r",
            "reg.internal:5000",
            creds("puller", "hunter2"),
        )
        .unwrap();
        assert!(
            pull_add(root, None, "r", "REG.example.com", creds("a", "b"))
                .unwrap_err()
                .to_string()
                .contains("already has pull credentials")
        );
        pull_set(
            root,
            None,
            "r",
            "reg.internal:5000",
            AuthChange {
                password: Some("${{obelisk:aws-ssm:/pull/pw}}".to_owned()),
                ..AuthChange::default()
            },
        )
        .unwrap();
        pull_remove(root, None, "r", "reg.example.com").unwrap();
        assert_eq!(
            std::fs::read_to_string(file(&dir)).unwrap(),
            "docker:\n  push:\n    prefix: reg.example.com/krrss\n    username: pusher\n    password: ${{obelisk:aws-ssm:/push/pw}}\n  pull:\n    auths:\n    - host: reg.internal:5000\n      username: puller\n      password: ${{obelisk:aws-ssm:/pull/pw}}\n"
        );

        push_unset_credentials(root, None, "r").unwrap();
        let shown = show(root, None, "r").unwrap().to_string();
        assert!(
            shown.contains("(no credentials: your own `docker login`)"),
            "{shown}"
        );
        assert!(
            shown.contains(
                "reg.internal:5000: username puller, password ${{obelisk:aws-ssm:/pull/pw}}"
            ),
            "{shown}"
        );

        remove(root, None, "r").unwrap();
        assert!(!file(&dir).exists());
        assert!(
            remove(root, None, "r")
                .unwrap_err()
                .to_string()
                .contains("doesn't exist")
        );
    }

    #[test]
    fn a_change_the_global_config_wont_load_is_undone() {
        let dir = root();
        let root = dir.path();
        push_set(
            root,
            None,
            "r",
            PushChange {
                prefix: Some("reg.example.com".to_owned()),
                ..PushChange::default()
            },
        )
        .unwrap();
        let before = std::fs::read_to_string(file(&dir)).unwrap();
        let error = push_set(
            root,
            None,
            "r",
            PushChange {
                prefix: Some("myuser/apps".to_owned()),
                ..PushChange::default()
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("doesn't start with a registry host"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(file(&dir)).unwrap(), before);

        // A new file that won't load is deleted again.
        let error = push_set(
            root,
            None,
            "k",
            PushChange {
                prefix: Some("https://x".to_owned()),
                ..PushChange::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("has a scheme"), "{error}");
        assert!(!root.join("clusters/k/docker.yaml").exists());
    }

    #[test]
    fn refuses_nothing_to_do_and_unknown_hosts() {
        let dir = root();
        let root = dir.path();
        assert!(
            push_set(root, None, "r", PushChange::default())
                .unwrap_err()
                .to_string()
                .starts_with("nothing to set")
        );
        assert!(
            pull_add(root, None, "r", "h", creds("u", "p"))
                .unwrap_err()
                .to_string()
                .contains("`config global docker push set --prefix …` creates it")
        );
        push_set(
            root,
            None,
            "r",
            PushChange {
                prefix: Some("reg.example.com".to_owned()),
                ..PushChange::default()
            },
        )
        .unwrap();
        assert!(
            pull_set(
                root,
                None,
                "r",
                "nope",
                AuthChange {
                    username: Some("u".to_owned()),
                    ..AuthChange::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("no pull credentials for `nope`")
        );
        assert!(
            push_unset_credentials(root, None, "r")
                .unwrap()
                .message
                .ends_with("nothing to change (no push credentials)")
        );
        assert_eq!(
            show(root, None, "k").unwrap().to_string(),
            format!(
                "cluster `k` has no {}/clusters/k/docker.yaml (its images are `k3d image import`ed instead)\n",
                root.display()
            )
        );
        assert!(
            show(root, None, "nope")
                .unwrap_err()
                .to_string()
                .contains("no cluster `nope`")
        );
    }

    /// `r`'s `docker.yaml`, set up with a prefix and one pull auth.
    fn with_docker(dir: &tempfile::TempDir, password: &str) {
        let root = dir.path();
        push_set(
            root,
            None,
            "r",
            PushChange {
                prefix: Some("reg.example.com".to_owned()),
                ..PushChange::default()
            },
        )
        .unwrap();
        pull_add(root, None, "r", "Reg.Example.com", creds("u", password)).unwrap();
    }

    #[test]
    fn a_change_that_changes_nothing_leaves_the_file_alone() {
        let dir = root();
        with_docker(&dir, "${{obelisk:aws-ssm:/p}}");
        let commented = format!("# mine\n{}", std::fs::read_to_string(file(&dir)).unwrap());
        std::fs::write(file(&dir), &commented).unwrap();
        let message = push_unset_credentials(dir.path(), None, "r")
            .unwrap()
            .message;
        assert!(
            message.ends_with("nothing to change (no push credentials)"),
            "{message}"
        );
        let change = AuthChange {
            username: Some("u".to_owned()),
            ..AuthChange::default()
        };
        // The host in any case; the message names it as written.
        let message = pull_set(dir.path(), None, "r", "reg.example.COM", change)
            .unwrap()
            .message;
        assert!(message.ends_with("for `Reg.Example.com`)"), "{message}");
        assert_eq!(std::fs::read_to_string(file(&dir)).unwrap(), commented);
    }

    #[test]
    fn writes_through_a_link_and_keeps_or_tightens_the_mode() {
        let dir = root();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("docker.yaml");
        std::fs::write(&target, "docker:\n  push: { prefix: reg.example.com }\n").unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, file(&dir)).unwrap();

        // An SSM-ref password: the mode stays.
        pull_add(
            dir.path(),
            None,
            "r",
            "reg.io",
            creds("u", "${{obelisk:aws-ssm:/p}}"),
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(file(&dir))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::read_to_string(&target).unwrap().contains("reg.io"));
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );

        // A plain-text one: only the owner can read it.
        let change = pull_add(dir.path(), None, "r", "reg2.io", creds("u", "hunter2")).unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            change.warnings[0].contains("plain-text password"),
            "{:?}",
            change.warnings
        );

        // Removing removes the link, not what it points to.
        remove(dir.path(), None, "r").unwrap();
        assert!(std::fs::symlink_metadata(file(&dir)).is_err());
        assert!(target.exists());
    }

    #[test]
    fn values_round_trip_however_they_need_quoting() {
        let dir = root();
        with_docker(&dir, "p");
        for value in [
            "123",
            "true",
            "null",
            "~",
            "yes",
            "012",
            "0x1F",
            " lead",
            "trail ",
            "a #b",
            "#x",
            "a: b",
            "- x",
            "*alias",
            "&anc",
            "!tag",
            "%",
            "@",
            "'q\"",
            "multi\nline",
            "${{obelisk:aws-ssm:/x}}",
            "$${{obelisk:lit}}",
            "{a}",
            "[b]",
            "é\\",
        ] {
            let change = AuthChange {
                password: Some(value.to_owned()),
                ..AuthChange::default()
            };
            pull_set(dir.path(), None, "r", "reg.example.com", change).unwrap();
            let config = GlobalConfig::load(dir.path(), None).unwrap();
            let docker = config.cluster("r").unwrap().docker.as_ref().unwrap();
            assert_eq!(docker.pull_auths[0].credentials.password, value);
        }
    }

    #[test]
    fn a_file_that_wont_load_can_still_be_removed() {
        let dir = root();
        std::fs::write(file(&dir), "docker:\n  push: { prefix: myuser/apps }\n").unwrap();
        assert!(show(dir.path(), None, "r").is_err());
        let change = remove(dir.path(), None, "r").unwrap();
        assert!(
            change.message.ends_with("removed it (it didn't load)"),
            "{}",
            change.message
        );
        assert!(!file(&dir).exists());
    }

    #[test]
    fn flags_are_checked_before_anything_is_written() {
        let dir = root();
        with_docker(&dir, "p");
        let before = std::fs::read_to_string(file(&dir)).unwrap();
        assert_eq!(
            pull_add(dir.path(), None, "r", "https://x", creds("u", "p"))
                .unwrap_err()
                .to_string(),
            "`--host`: `https://x` must be just a host (and port): no scheme or path"
        );
        assert_eq!(
            pull_add(dir.path(), None, "r", "x.io", creds("u", ""))
                .unwrap_err()
                .to_string(),
            "`--password` is empty"
        );
        let prefix = PushChange {
            prefix: Some("myuser/apps".to_owned()),
            ..PushChange::default()
        };
        assert!(
            push_set(dir.path(), None, "r", prefix)
                .unwrap_err()
                .to_string()
                .starts_with("`--prefix`: ")
        );
        let only_credentials = PushChange {
            credentials: Some(creds("u", "${{obelisk:aws-ssm:/p}}")),
            ..PushChange::default()
        };
        let message = push_set(dir.path(), None, "r", only_credentials)
            .unwrap()
            .message;
        assert!(message.ends_with("set the push credentials"), "{message}");
        assert_ne!(std::fs::read_to_string(file(&dir)).unwrap(), before);
    }

    #[test]
    fn a_broken_file_is_found_by_the_clusters_name_and_a_dangling_link_is_handled() {
        let dir = root();
        // Directory `dirc` holds cluster `named`.
        let named = dir.path().join("clusters/dirc");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(
            named.join("cluster.yaml"),
            "cluster:\n  name: named\n  k8s: k3d\n  location: local\n  managed: false\n",
        )
        .unwrap();
        std::fs::write(
            named.join("docker.yaml"),
            "docker:\n  push: { prefix: nope }\n",
        )
        .unwrap();
        remove(dir.path(), None, "named").unwrap();
        assert!(!named.join("docker.yaml").exists());
        assert!(cluster_dir(dir.path(), "../x").is_none());

        // A link to nothing: `push set` creates what it points to; `remove` removes the link.
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("docker.yaml");
        std::os::unix::fs::symlink(&target, file(&dir)).unwrap();
        assert!(show(dir.path(), None, "r").unwrap().docker.is_none());
        let message = remove(dir.path(), None, "r").unwrap().message;
        assert!(
            message.ends_with("removed it (a link to nothing)"),
            "{message}"
        );
        std::os::unix::fs::symlink(&target, file(&dir)).unwrap();
        let prefix = PushChange {
            prefix: Some("reg.io".to_owned()),
            ..PushChange::default()
        };
        push_set(dir.path(), None, "r", prefix.clone()).unwrap();
        assert!(target.exists());
        assert!(
            std::fs::symlink_metadata(file(&dir))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let message = push_set(dir.path(), None, "r", prefix).unwrap().message;
        assert!(
            message.ends_with("nothing to change (set the push prefix `reg.io`)"),
            "{message}"
        );
    }
}
