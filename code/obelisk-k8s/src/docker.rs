//! Running things that build images: an image's `preBuild`, then `docker build`. What they print
//! goes to stderr (stdout is for the report), as they print it.

use std::ffi::OsString;
use std::os::fd::AsFd;
use std::path::Path;
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
    command.current_dir(&pre.cwd);
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
    fn pre_build_runs_in_its_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pre.sh");
        write_script(&script, "#!/bin/sh\npwd > seen\n");
        let pre = |run| {
            image(Some(PreBuild {
                run,
                cwd: dir.path().to_owned(),
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
}
