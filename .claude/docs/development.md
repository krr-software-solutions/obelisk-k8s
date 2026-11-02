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
| `gitlab-release`  | —      | auto | archives → the generic package registry, and the GitLab release   |
| `install-smoke`   | —      | auto | `install.sh` installs that version from the release (alpine, debian) |

**Releasing:** tag the merged `main`, `git tag v1.2.3 && git push origin v1.2.3`. `Cargo.toml` keeps
the `0.0.0` placeholder; release jobs stamp the tag's version in with `scripts/set-version.sh`. The
project, its packages and releases are private: installing needs a token (`install.sh`, and the
`curl … | sh` line in the README). Mirroring to a public GitHub release (as obeliskd does) is a TODO.

## The `FIX-PRE-MERGE` marker

Leave `FIX-PRE-MERGE` in a comment for anything that must be fixed before merge. CI fails while
any remain (`ALLOW_FIX_PRE_MERGE=true` lets one through). `scripts/check-for-fix.sh` skips itself
and `.claude/`, so don't write the literal marker anywhere else (CI comments included) unless you
mean it.

## Repo

- GitLab: `gitlab.krr-software-solutions.com/oss/obelisk-k8s` (project ID 24), default branch `main`.
- Merge via MR; squash to a single clean commit before merging.
