//! Running things that build and deliver images: an image's `preBuild`, `docker build`, and
//! `docker login` / `tag` / `push` or `k3d image import`. What they print
//! goes to stderr (stdout is for the report), as they print it.

use std::ffi::OsString;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ObeliskK8sError;
use crate::config::build::in_repo;
use crate::config::images::{Dockerfile, Image, Run};

/// Runs `image`'s `preBuild` (if any) in its `cwd`; errors show its program relative to
/// `repo_root`.
pub(crate) fn pre_build(image: &Image, repo_root: &Path) -> Result<(), ObeliskK8sError> {
    let Some(pre) = &image.pre_build else {
        return Ok(());
    };
    let (mut command, program) = match &pre.run {
        Run::File { path } => (Command::new(path), path.as_path()),
        Run::Command { command: words } => {
            let mut command = Command::new(&words[0]);
            command.args(&words[1..]);
            (command, Path::new(&words[0]))
        }
    };
    command.current_dir(&pre.cwd).envs(&pre.env);
    let what = format!(
        "the `preBuild` of image `{}` ({})",
        image.tag(),
        in_repo(repo_root, program).display()
    );
    run(&mut command, &what)
}

/// The arguments to `docker` that build `image`.
fn build_args(image: &Image) -> Vec<OsString> {
    let Dockerfile::File { path } = &image.dockerfile;
    vec![
        "build".into(),
        "--file".into(),
        path.into(),
        "--tag".into(),
        image.tag().into(),
        image.context_path.clone().into(),
    ]
}

/// Builds `image` with `docker build`.
pub(crate) fn build(image: &Image) -> Result<(), ObeliskK8sError> {
    let mut command = Command::new("docker");
    command.args(build_args(image));
    run(
        &mut command,
        &format!("`docker build` of image `{}`", image.tag()),
    )
}

/// Push credentials for one registry: a throwaway docker client config holding just them, that
/// still reaches the user's daemon (a client config also picks the daemon, through its current
/// context: so the user's context, and the context store it lives in, carry over, TLS settings
/// and all). Dropping it deletes the config.
#[derive(Debug)]
pub(crate) struct PushAuth {
    dir: tempfile::TempDir,
}

/// The user's docker client: its config dir, and its current context (`None`: the default one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserDocker {
    pub(crate) config_dir: PathBuf,
    pub(crate) context: Option<String>,
}

impl UserDocker {
    /// Asks the user's docker client.
    pub(crate) fn current() -> Result<Self, ObeliskK8sError> {
        let config_dir = std::env::var_os("DOCKER_CONFIG").map_or_else(
            || {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from(".docker"),
                    |home| Path::new(&home).join(".docker"),
                )
            },
            PathBuf::from,
        );
        // Absolute: the context store is linked to from another directory.
        let config_dir =
            std::path::absolute(&config_dir).map_err(|source| ObeliskK8sError::Io {
                path: config_dir.clone(),
                source,
            })?;
        let what = "`docker context show`";
        let output = Command::new("docker")
            .args(["context", "show"])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| ObeliskK8sError::Image(format!("{what} can't run: {error}")))?;
        if !output.status.success() {
            return Err(ObeliskK8sError::Image(format!(
                "{what} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let context = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Self {
            config_dir,
            context: (!context.is_empty() && context != "default").then_some(context),
        })
    }
}

impl PushAuth {
    /// A client config in a new directory under `parent` with `username` and `password` for
    /// registry `host` (a file, mode 0600: never in argv), on `user`'s context.
    pub(crate) fn new(
        parent: &Path,
        user: &UserDocker,
        host: &str,
        username: &str,
        password: &str,
    ) -> Result<Self, ObeliskK8sError> {
        use base64::Engine as _;
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let io = |path: &Path, source| ObeliskK8sError::Io {
            path: path.to_owned(),
            source,
        };
        let dir = tempfile::Builder::new()
            .prefix("obelisk-k8s-docker-")
            .tempdir_in(parent)
            .map_err(|source| io(parent, source))?;
        let auth =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let mut config = serde_json::json!({ "auths": { host: { "auth": auth } } });
        if let Some(context) = &user.context {
            config["currentContext"] = serde_json::Value::String(context.clone());
            let store = user.config_dir.join("contexts");
            if !store.is_dir() {
                return Err(ObeliskK8sError::Image(format!(
                    "docker context `{context}` is current, but there's no context store at {}",
                    store.display()
                )));
            }
            let link = dir.path().join("contexts");
            std::os::unix::fs::symlink(&store, &link).map_err(|source| io(&link, source))?;
        }
        let path = dir.path().join("config.json");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| io(&path, source))?;
        file.write_all(config.to_string().as_bytes())
            .map_err(|source| io(&path, source))?;
        Ok(Self { dir })
    }

    /// Where throwaway configs go: `$XDG_RUNTIME_DIR` (per-user, in memory) when there is one.
    pub(crate) fn parent() -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir)
    }
}

/// The arguments and environment that push `remote` (with `auth`'s config, if any).
fn push_command(auth: Option<&PushAuth>, remote: &str) -> Command {
    let mut command = Command::new("docker");
    if let Some(auth) = auth {
        command.arg("--config").arg(auth.dir.path());
    }
    command.args(["push", remote]);
    command
}

/// Tags local image `local` as `remote` (with the user's own client), then pushes `remote` (with
/// `auth`, if any, else the user's own login).
pub(crate) fn push(
    auth: Option<&PushAuth>,
    local: &str,
    remote: &str,
) -> Result<(), ObeliskK8sError> {
    run(
        Command::new("docker").args(["tag", local, remote]),
        &format!("`docker tag {local} {remote}`"),
    )?;
    run(
        &mut push_command(auth, remote),
        &format!("`docker push {remote}`"),
    )
}

/// Loads local image `tag` into k3d cluster `cluster`'s nodes.
pub(crate) fn k3d_import(cluster: &str, tag: &str) -> Result<(), ObeliskK8sError> {
    run(
        Command::new("k3d").args(["image", "import", tag, "--cluster", cluster]),
        &format!("`k3d image import {tag}` into cluster `{cluster}`"),
    )
}

/// Runs `command` (named `what` in errors), its stdout and stderr on our stderr.
fn run(command: &mut Command, what: &str) -> Result<(), ObeliskK8sError> {
    // Never our stdout, which is for the report: if stderr can't be shared, don't run at all.
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| ObeliskK8sError::Image(format!("{what} can't run: {error}")))?;
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| ObeliskK8sError::Image(format!("{what} can't run: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(ObeliskK8sError::Image(format!("{what} failed ({status})")))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::images::PreBuild;
    use crate::config::test_support::write_script;

    fn image(pre_build: Option<PreBuild>) -> Image {
        Image {
            name: "app".to_owned(),
            version: "1.2.3".to_owned(),
            pre_build,
            dockerfile: Dockerfile::File {
                path: "/repo/units/app/Dockerfile".into(),
            },
            context_path: "/repo".into(),
        }
    }

    #[test]
    fn builds_with_the_dockerfile_tag_and_context() {
        assert_eq!(
            build_args(&image(None)),
            [
                "build",
                "--file",
                "/repo/units/app/Dockerfile",
                "--tag",
                "app:1.2.3",
                "/repo"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn pre_build_runs_in_its_cwd_with_its_env() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pre.sh");
        write_script(&script, "#!/bin/sh\npwd > seen\necho \"$MODE\" > mode\n");
        let pre = |run| {
            image(Some(PreBuild {
                run,
                cwd: dir.path().to_owned(),
                env: BTreeMap::from([("MODE".to_owned(), "release".to_owned())]),
            }))
        };
        pre_build(
            &pre(Run::File {
                path: script.clone(),
            }),
            dir.path(),
        )
        .unwrap();
        let seen = std::fs::read_to_string(dir.path().join("seen")).unwrap();
        // Canonical on both sides: the temp dir may be under a symlink (macOS's `/var`).
        assert_eq!(
            Path::new(seen.trim()).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        let mode = std::fs::read_to_string(dir.path().join("mode")).unwrap();
        assert_eq!(mode, "release\n");

        let words = vec!["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()];
        let error = pre_build(&pre(Run::Command { command: words }), dir.path())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the `preBuild` of image `app:1.2.3` (sh) failed (exit status: 3)"
        );
        assert!(pre_build(&image(None), dir.path()).is_ok());
    }

    #[test]
    fn push_auth_is_a_private_config_on_the_users_context() {
        use base64::Engine as _;
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("contexts")).unwrap();
        let user = UserDocker {
            config_dir: home.path().to_owned(),
            context: Some("colima".to_owned()),
        };
        let auth = PushAuth::new(parent.path(), &user, "reg.example.com", "me", "pa:ss").unwrap();

        // Just the credentials and the user's context, readable only by us; never an argument.
        let path = auth.dir.path().join("config.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode("me:pa:ss");
        assert_eq!(
            config,
            serde_json::json!({
                "auths": { "reg.example.com": { "auth": encoded } },
                "currentContext": "colima",
            })
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // The context store, with its TLS material, is the user's.
        assert_eq!(
            std::fs::read_link(auth.dir.path().join("contexts")).unwrap(),
            home.path().join("contexts")
        );

        let command = push_command(Some(&auth), "reg.example.com/krrss/app:1");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("--config"),
                auth.dir.path().as_os_str(),
                "push".as_ref(),
                "reg.example.com/krrss/app:1".as_ref()
            ]
        );
        let plain = push_command(None, "reg.io/app:1");
        assert_eq!(
            plain.get_args().collect::<Vec<_>>(),
            ["push", "reg.io/app:1"]
        );

        // On the default context: nothing to carry over.
        let default = UserDocker {
            context: None,
            ..user
        };
        let auth = PushAuth::new(parent.path(), &default, "reg.io", "me", "pw").unwrap();
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(auth.dir.path().join("config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(config.get("currentContext"), None);
        assert!(!auth.dir.path().join("contexts").exists());
    }
}
