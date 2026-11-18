//! Git refs, resolved to commits; the detached worktrees an env renders from; and a bare repo's
//! own config, read without one.
//!
//! A ref is written as a chain of candidates, `a|b|c`: the first that names a commit wins. A
//! candidate that starts with one of the repo's remotes (`origin/feature/abc`, `origin/HEAD`) is
//! looked up only after fetching the repo (every remote, `git fetch --all`), at most once per repo
//! per run and only if a chain gets that far (unless fetching is off), so a stale remote-tracking
//! ref doesn't win; any other candidate (a local branch, a tag, a sha) is looked up as it is,
//! without the network. Code has to be committed to be deployed; it only has to be pushed when the
//! chain names a remote.
//!
//! A commit is checked out as a detached worktree of its registered repo (bare or not), at
//! `<global root>/envs/target/<env>/<repo name>/<full sha>/`: each env has its own, reused while it
//! keeps resolving to that commit.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ObeliskK8sError;

/// `a|b|c`: candidates for one commit, tried in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChain(Vec<String>);

impl RefChain {
    /// Parses `a|b|c` (spaces around each `|` are dropped).
    ///
    /// # Errors
    ///
    /// A message (for the caller to place) if a candidate is empty, or has whitespace in it, or
    /// starts with `-` (git would read it as an option).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut candidates = Vec::new();
        for candidate in text.split('|').map(str::trim) {
            if candidate.is_empty() {
                return Err(format!(
                    "ref `{text}` has an empty candidate (write `a|b|c`)"
                ));
            }
            if candidate.contains(char::is_whitespace) || candidate.starts_with('-') {
                return Err(format!(
                    "ref `{text}`: `{candidate}` isn't a ref (no whitespace, and no leading `-`)"
                ));
            }
            candidates.push(candidate.to_owned());
        }
        Ok(Self(candidates))
    }

    #[must_use]
    pub fn candidates(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for RefChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("|"))
    }
}

/// A chain resolved: the candidate that won, and its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub candidate: String,
    /// The full sha.
    pub sha: String,
}

/// Which repos have been fetched this run (each at most once), and whether fetching is allowed
/// at all. One is shared by everything a run resolves.
#[derive(Debug)]
pub struct Fetcher {
    enabled: bool,
    fetched: HashSet<PathBuf>,
}

impl Fetcher {
    /// `enabled: false` looks remote candidates up as they are (offline: possibly stale).
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            fetched: HashSet::new(),
        }
    }
}

/// Resolves `chain` in the repo at `git_dir` (named `repo` in messages). `progress` hears about
/// each fetch as it starts.
///
/// # Errors
///
/// [`ObeliskK8sError::Git`] if `git` can't run, a fetch fails, or no candidate names a commit.
pub fn resolve(
    git_dir: &Path,
    repo: &str,
    chain: &RefChain,
    fetcher: &mut Fetcher,
    progress: &mut dyn FnMut(&str),
) -> Result<Resolved, ObeliskK8sError> {
    let remotes = remotes(git_dir)?;
    for candidate in chain.candidates() {
        let remote = candidate
            .split_once('/')
            .map(|(first, _)| first)
            .filter(|first| remotes.iter().any(|r| r == first));
        if remote.is_some() && fetcher.enabled && fetcher.fetched.insert(git_dir.to_owned()) {
            progress(&format!("fetching repo `{repo}`..."));
            git(git_dir, &["fetch", "--quiet", "--prune", "--all"])
                .map_err(|e| ObeliskK8sError::Git(format!("repo `{repo}`: {e}")))?;
        }
        if let Some(sha) = commit_of(git_dir, candidate)? {
            return Ok(Resolved {
                candidate: candidate.clone(),
                sha,
            });
        }
    }
    let offline = if fetcher.enabled {
        ""
    } else {
        " (fetching is off)"
    };
    Err(ObeliskK8sError::Git(format!(
        "repo `{repo}`: no commit for ref `{chain}`{offline}: commit (and push, for a remote candidate) first"
    )))
}

/// The full sha `candidate` names, if it names a commit.
fn commit_of(git_dir: &Path, candidate: &str) -> Result<Option<String>, ObeliskK8sError> {
    let spec = format!("{candidate}^{{commit}}");
    let output = git_command(git_dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])
        .output()
        .map_err(|e| ObeliskK8sError::Git(format!("can't run `git`: {e}")))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn remotes(git_dir: &Path) -> Result<Vec<String>, ObeliskK8sError> {
    let out = git(git_dir, &["remote"]).map_err(|e| {
        ObeliskK8sError::Git(format!("{}: not a git repo? ({e})", git_dir.display()))
    })?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Where env `env`'s checkouts go, under the global config's real directory.
#[must_use]
pub fn env_dir(global_dir: &Path, env: &str) -> PathBuf {
    global_dir.join("envs").join("target").join(env)
}

/// Where env `env`'s checkout of `sha` in repo `repo` goes.
#[must_use]
pub fn checkout_dir(global_dir: &Path, env: &str, repo: &str, sha: &str) -> PathBuf {
    env_dir(global_dir, env).join(repo).join(sha)
}

/// Whether `name` looks like a full sha (a checkout's directory name).
#[must_use]
pub fn is_full_sha(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Removes the checkout at `dir`: through `git worktree remove --force` in `git_dir` (its repo,
/// if it's still registered), else by deleting it (its repo's `git worktree prune` tidies up).
///
/// # Errors
///
/// [`ObeliskK8sError::Io`] if it can't be deleted.
pub fn remove_checkout(git_dir: Option<&Path>, dir: &Path) -> Result<(), ObeliskK8sError> {
    let dir_text = dir.to_string_lossy();
    if let Some(git_dir) = git_dir
        && git(git_dir, &["worktree", "remove", "--force", &dir_text]).is_ok()
    {
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|source| ObeliskK8sError::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    if let Some(git_dir) = git_dir {
        let _ = git(git_dir, &["worktree", "prune"]);
    }
    Ok(())
}

/// Makes sure `dir` is a detached worktree of `git_dir` at `sha`: reuses it if it already is,
/// else adds it. Returns whether it was added.
///
/// # Errors
///
/// [`ObeliskK8sError::Git`] if `dir` is there but isn't a checkout of `sha`, or `git worktree add`
/// fails; [`ObeliskK8sError::Io`] if its parent can't be made.
pub fn ensure_checkout(
    git_dir: &Path,
    repo: &str,
    sha: &str,
    dir: &Path,
) -> Result<bool, ObeliskK8sError> {
    if dir.exists() {
        return match git(dir, &["rev-parse", "HEAD"]) {
            Ok(head) if head == sha => Ok(false),
            _ => Err(ObeliskK8sError::Git(format!(
                "{} is there but isn't a checkout of {sha}: remove it (`git -C {} worktree remove --force {}`, or `rm -r` it and `git worktree prune`)",
                dir.display(),
                git_dir.display(),
                dir.display()
            ))),
        };
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ObeliskK8sError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let dir_text = dir.to_string_lossy();
    git(
        git_dir,
        &["worktree", "add", "--quiet", "--detach", &dir_text, sha],
    )
    .map_err(|e| ObeliskK8sError::Git(format!("repo `{repo}`: checking out {sha}: {e}")))?;
    Ok(true)
}

/// Whether `path` is a git repo (bare, or a work tree's root or anywhere in one).
pub(crate) fn is_git_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--git-dir"]).is_ok()
}

/// The ref a repo's own config is read at, when it isn't checked out (a bare repo, or a checkout
/// without `.obelisk-k8s/`): `origin/HEAD` (what the remote's default branch
/// says, as of the last fetch) if it has one, else `HEAD` (often a local branch nothing updates).
pub(crate) fn config_ref(git_dir: &Path) -> &'static str {
    if commit_of(git_dir, "refs/remotes/origin/HEAD").is_ok_and(|sha| sha.is_some()) {
        "origin/HEAD"
    } else {
        "HEAD"
    }
}

/// Writes `dir` (a directory in the tree) at `rev`, from the repo at `git_dir`, under `dest`
/// (`git archive | tar -x`): no worktree, nothing registered, nothing left in the repo.
pub(crate) fn extract(git_dir: &Path, rev: &str, dir: &str, dest: &Path) -> Result<(), String> {
    use std::process::Stdio;
    let mut archive = git_command(git_dir)
        .args(["archive", "--format=tar", rev, "--", dir])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run `git`: {e}"))?;
    let tar_in = archive.stdout.take().ok_or("`git archive` has no stdout")?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(tar_in)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("can't run `tar`: {e}"))?;
    let archived = archive
        .wait_with_output()
        .map_err(|e| format!("`git archive` failed: {e}"))?;
    if !archived.status.success() {
        let stderr = String::from_utf8_lossy(&archived.stderr);
        return Err(format!(
            "`git archive {rev} -- {dir}` failed: {}",
            stderr.trim()
        ));
    }
    if !tar.status.success() {
        let stderr = String::from_utf8_lossy(&tar.stderr);
        return Err(format!("`tar -x` failed: {}", stderr.trim()));
    }
    Ok(())
}

/// `path` at `rev`'s text, from the repo at `git_dir`.
pub(crate) fn show(git_dir: &Path, rev: &str, path: &str) -> Result<String, String> {
    git(git_dir, &["show", &format!("{rev}:{path}")])
}

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// Runs `git args` in `dir`: its stdout (trimmed), or what went wrong.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_command(dir)
        .args(args)
        .output()
        .map_err(|e| format!("can't run `git`: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs git in `dir` (asserting it worked), as a fixed test identity.
    pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
        let output = git_command(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// An upstream repo with `files` committed on `main`, and a bare clone of it (with
    /// remote-tracking refs, as a fetch-configured bare checkout has).
    pub(crate) fn upstream_and_bare(root: &Path, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let upstream = root.join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        run(&upstream, &["init", "-q"]);
        commit(&upstream, files, "one");
        let bare = root.join("bare");
        run(
            root,
            &["clone", "-q", "--bare", upstream.to_str().unwrap(), "bare"],
        );
        run(
            &bare,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        run(&bare, &["fetch", "-q", "origin"]);
        (upstream, bare)
    }

    pub(crate) fn commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "--allow-empty", "-m", message]);
        run(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn parses_chains() {
        let chain = RefChain::parse("feature/abc | origin/feature/abc|origin/HEAD").unwrap();
        assert_eq!(
            chain.candidates(),
            ["feature/abc", "origin/feature/abc", "origin/HEAD"]
        );
        assert_eq!(
            chain.to_string(),
            "feature/abc|origin/feature/abc|origin/HEAD"
        );
        assert!(RefChain::parse("a||b").is_err());
        assert!(RefChain::parse("").is_err());
        assert!(RefChain::parse("-x").is_err());
        assert!(RefChain::parse("a b").is_err());
    }

    #[test]
    fn a_local_candidate_wins_before_anything_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, bare) = upstream_and_bare(dir.path(), &[("a", "1")]);
        let first = run(&bare, &["rev-parse", "main"]);
        // Upstream moves on; the bare repo hasn't fetched it.
        let second = commit(&upstream, &[("a", "2")], "two");

        let mut fetcher = Fetcher::new(true);
        let mut steps = Vec::new();
        let chain = RefChain::parse("nope|main|origin/main").unwrap();
        let resolved = resolve(&bare, "r", &chain, &mut fetcher, &mut |s| {
            steps.push(s.to_owned());
        })
        .unwrap();
        assert_eq!(resolved.candidate, "main");
        assert_eq!(resolved.sha, first);
        assert!(steps.is_empty(), "{steps:?}");

        // A remote candidate fetches first, so it's never stale.
        let chain = RefChain::parse("nope|origin/main").unwrap();
        let resolved = resolve(&bare, "r", &chain, &mut fetcher, &mut |s| {
            steps.push(s.to_owned());
        })
        .unwrap();
        assert_eq!(resolved.sha, second);
        assert_eq!(steps.len(), 1);
        // Once per repo per run.
        resolve(&bare, "r", &chain, &mut fetcher, &mut |s| {
            steps.push(s.to_owned());
        })
        .unwrap();
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn without_fetching_a_remote_candidate_is_used_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, bare) = upstream_and_bare(dir.path(), &[("a", "1")]);
        let first = run(&bare, &["rev-parse", "origin/main"]);
        commit(&upstream, &[("a", "2")], "two");
        let chain = RefChain::parse("origin/main").unwrap();
        let resolved = resolve(&bare, "r", &chain, &mut Fetcher::new(false), &mut |_| {}).unwrap();
        assert_eq!(resolved.sha, first);
    }

    #[test]
    fn nothing_resolving_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_, bare) = upstream_and_bare(dir.path(), &[("a", "1")]);
        let chain = RefChain::parse("nope|origin/nope").unwrap();
        let error = resolve(&bare, "r", &chain, &mut Fetcher::new(true), &mut |_| {}).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no commit for ref `nope|origin/nope`"),
            "{error}"
        );
    }

    #[test]
    fn checks_out_detached_and_reuses_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let (_, bare) = upstream_and_bare(dir.path(), &[("a", "1")]);
        let sha = run(&bare, &["rev-parse", "main"]);
        let out = checkout_dir(&dir.path().join("global"), "e", "r", &sha);
        assert!(ensure_checkout(&bare, "r", &sha, &out).unwrap());
        assert_eq!(std::fs::read_to_string(out.join("a")).unwrap(), "1");
        assert_eq!(run(&out, &["rev-parse", "HEAD"]), sha);
        let on_branch = git_command(&out)
            .args(["symbolic-ref", "-q", "HEAD"])
            .status()
            .unwrap()
            .success();
        assert!(!on_branch, "detached");
        assert!(!ensure_checkout(&bare, "r", &sha, &out).unwrap(), "reused");

        std::fs::create_dir_all(dir.path().join("junk")).unwrap();
        let error = ensure_checkout(&bare, "r", &sha, &dir.path().join("junk")).unwrap_err();
        assert!(error.to_string().contains("isn't a checkout of"), "{error}");
    }
}
