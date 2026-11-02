# Development

## Commands

```sh
cargo run -q -p obelisk-k8s-cli -- --help             # run the CLI
(cd .tmp-scaffold/.repo-root && cargo run -q -p obelisk-k8s-cli -- config repo show)
cargo fmt --all                                        # format
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The tests that render Go templates run `helm`, so they need it on the `PATH`; without it they fail
unless `OBELISK_TEST_SKIP_HELM=1` is set.

`--locked` matches what CI will run: if `Cargo.lock` needs to change, commit the change.

## CI (`.gitlab-ci.yml`)

MR pipelines, and a release from a version tag (as obeliskd). Jobs come from the shared templates in
`krrss/util/pipeline-utils` (`.runner`, `".mr=…,tag=…"`).

| Job               | MR     | Tag  | What                                                              |
|-------------------|--------|------|-------------------------------------------------------------------|
| `check-pre-merge` | auto   | —    | fails if a `FIX-PRE-MERGE` marker is left anywhere (see below)    |
| `check`           | auto   | auto | fmt, clippy, tests (`OBELISK_TEST_SKIP_HELM=1`: no `helm` in CI)  |
| `shellcheck`      | auto   | —    | `install.sh` and `scripts/`                                       |
| `build`           | manual | auto | the binary per target (cargo-zigbuild): Linux musl, macOS; x86_64/aarch64 |
| `gitlab-release`  | —      | auto | archives → the generic package registry, and the GitLab release (private) |
| `github-release`  | —      | auto | the tag, archives and `install.sh` → the public GitHub mirror's release |
| `install-smoke`   | —      | auto | `install.sh` from the GitHub release installs that version (alpine, debian) |

**Releasing:** tag the merged `main`, `git tag v1.2.3 && git push origin v1.2.3`. `Cargo.toml` keeps
the `0.0.0` placeholder; release jobs stamp the tag's version in with `scripts/set-version.sh`. GitLab
stays private (code, packages, its release); the public face is the GitHub mirror,
`github.com/krr-software-solutions/obelisk-k8s`: GitLab push-mirrors the code to it, and
`github-release` (with the `GITHUB_TOKEN` CI variable) publishes each release there, with
version-less asset names so `releases/latest/download/…` (what `install.sh` and the README use)
always works. A pre-release tag (`v1.2.3-rc.1`) is marked one, so it's never `latest`. Releases before
`v0.1.1` (just `v0.1.0`) are on GitLab only.

## The `FIX-PRE-MERGE` marker

Leave `FIX-PRE-MERGE` in a comment for anything that must be fixed before merge. CI fails while
any remain (`ALLOW_FIX_PRE_MERGE=true` lets one through). `scripts/check-for-fix.sh` skips itself
and `.claude/`, so don't write the literal marker anywhere else (CI comments included) unless you
mean it.

## Repo

- GitLab: `gitlab.krr-software-solutions.com/oss/obelisk-k8s` (project ID 24), default branch `main`.
- GitHub (public mirror, releases): `github.com/krr-software-solutions/obelisk-k8s`.
- Merge via MR; squash to a single clean commit before merging.
