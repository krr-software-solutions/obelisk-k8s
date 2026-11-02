//! What git says about a repo, for the `obelisk.git` block.

use std::path::Path;
use std::process::Command;

use serde_json::{Map, Value, json};

/// `obelisk.git` for the repo at `repo_root`: `null` if it isn't a git repo with a commit (or `git`
/// isn't installed), else
///
/// ```yaml
/// sha: { short, value }          # HEAD
/// tag: { exact, value }          # `git describe --tags --exact-match` / `git describe --tags`; null without
/// dirty: { tracked, untracked, any }
/// ```
///
/// `dirty.tracked` is any change to tracked files, staged or not; `dirty.untracked` is any untracked
/// file that isn't ignored. `dirty` is `null` if `git status` can't say (e.g. a bare repo): never a
/// made-up "clean".
///
/// The repo is whichever one `repo_root` is in: if it's a subdirectory of a bigger repo, that's the
/// one (its HEAD, and dirt anywhere in it). `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` from the
/// environment are ignored, so a git hook or CI can't point this elsewhere. Any tag counts (not only
/// version-like ones): with several on HEAD, `git describe` prefers an annotated one, then the first by
/// name.
#[must_use]
pub(crate) fn facts(repo_root: &Path) -> Value {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .ok()?;
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned()
        })
    };
    let (Some(sha), Some(short)) = (
        git(&["rev-parse", "HEAD"]),
        git(&["rev-parse", "--short", "HEAD"]),
    ) else {
        return Value::Null;
    };
    let dirty = git(&["status", "--porcelain=v1", "--untracked-files=normal"]).map_or(
        Value::Null,
        |status| {
            let untracked = status.lines().any(|l| l.starts_with("??"));
            let tracked = status
                .lines()
                .any(|l| !l.starts_with("??") && !l.is_empty());
            json!({ "tracked": tracked, "untracked": untracked, "any": tracked || untracked })
        },
    );

    let mut facts = Map::new();
    facts.insert("sha".to_owned(), json!({ "short": short, "value": sha }));
    facts.insert(
        "tag".to_owned(),
        json!({
            "exact": git(&["describe", "--tags", "--exact-match", "HEAD"]),
            "value": git(&["describe", "--tags"]),
        }),
    );
    facts.insert("dirty".to_owned(), dirty);
    Value::Object(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn reads_sha_tags_and_dirt() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(facts(d), Value::Null, "not a repo yet");
        git(d, &["init", "-q"]);
        std::fs::write(d.join("a"), "1").unwrap();
        git(d, &["add", "a"]);
        git(d, &["commit", "-q", "-m", "one"]);

        let f = facts(d);
        let sha = f["sha"]["value"].as_str().unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.starts_with(f["sha"]["short"].as_str().unwrap()));
        assert_eq!(f["tag"], json!({ "exact": null, "value": null }));
        assert_eq!(
            f["dirty"],
            json!({ "tracked": false, "untracked": false, "any": false })
        );

        git(d, &["tag", "v1.2.3"]);
        assert_eq!(
            facts(d)["tag"],
            json!({ "exact": "v1.2.3", "value": "v1.2.3" })
        );

        std::fs::write(d.join("b"), "2").unwrap();
        git(d, &["add", "b"]);
        git(d, &["commit", "-q", "-m", "two"]);
        let f = facts(d);
        assert_eq!(f["tag"]["exact"], Value::Null);
        let value = f["tag"]["value"].as_str().unwrap();
        assert!(value.starts_with("v1.2.3-1-g"), "{value}");

        std::fs::write(d.join("new"), "").unwrap();
        assert_eq!(
            facts(d)["dirty"],
            json!({ "tracked": false, "untracked": true, "any": true })
        );
        std::fs::write(d.join("a"), "changed").unwrap();
        assert_eq!(
            facts(d)["dirty"],
            json!({ "tracked": true, "untracked": true, "any": true })
        );
    }

    #[test]
    fn ignores_ignored_files_and_works_detached() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q"]);
        std::fs::write(d.join(".gitignore"), "target/\n").unwrap();
        git(d, &["add", ".gitignore"]);
        git(d, &["commit", "-q", "-m", "one"]);
        std::fs::create_dir(d.join("target")).unwrap();
        std::fs::write(d.join("target/x"), "").unwrap();
        assert_eq!(facts(d)["dirty"]["any"], false, "ignored files aren't dirt");

        let sha = facts(d)["sha"]["value"].clone();
        git(d, &["checkout", "-q", "--detach"]);
        assert_eq!(facts(d)["sha"]["value"], sha);
    }

    #[test]
    fn a_status_that_fails_is_unknown_not_clean() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "src"]);
        std::fs::write(d.join("src/a"), "1").unwrap();
        git(&d.join("src"), &["add", "a"]);
        git(&d.join("src"), &["commit", "-q", "-m", "one"]);
        git(d, &["clone", "-q", "--bare", "src", "bare"]);
        // A bare repo has a HEAD but no work tree to look at.
        let f = facts(&d.join("bare"));
        assert!(f["sha"]["value"].is_string(), "{f}");
        assert_eq!(f["dirty"], Value::Null);
    }
}
