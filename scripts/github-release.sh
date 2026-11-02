#!/bin/sh
# Publish the release on the public GitHub mirror: push the tag there (GitLab's push mirror may
# not have caught up yet), then create the release with the archives from dist/.
#
# On GitHub the assets drop the version, so the latest release has stable download URLs:
#   https://github.com/krr-software-solutions/obelisk-k8s/releases/latest/download/obelisk-k8s-x86_64-unknown-linux-musl.tar.gz
# (each with a matching .sha256), which is what install.sh (also attached) downloads. Re-running
# it re-uploads the assets of an existing release.
#
# Run by CI's `github-release` job: needs git + gh, a full clone (GIT_DEPTH 0), and GITHUB_TOKEN,
# a token that can write the repo's contents.
set -eu

repo="${GITHUB_REPO:-krr-software-solutions/obelisk-k8s}"
tag="${CI_COMMIT_TAG:?CI_COMMIT_TAG is not set}"
: "${GITHUB_TOKEN:?GITHUB_TOKEN is not set (a token that can write github.com/$repo)}"
version="${tag#v}"
cd "$(dirname "$0")/.."

# gh as git's credential helper, so the token never appears in a URL or the job log.
gh auth setup-git
git push "https://github.com/$repo.git" "refs/tags/$tag"

assets="$(mktemp -d)"
trap 'rm -rf "$assets"' EXIT
for file in dist/*.tar.gz; do
  # obelisk-k8s-1.2.3-x86_64-unknown-linux-musl.tar.gz → obelisk-k8s-x86_64-unknown-linux-musl.tar.gz
  name="$(basename "$file" | sed "s/^obelisk-k8s-$version-/obelisk-k8s-/")"
  cp "$file" "$assets/$name"
  (cd "$assets" && sha256sum "$name" > "$name.sha256")
done
cp install.sh "$assets/install.sh"

if gh release view "$tag" --repo "$repo" > /dev/null 2>&1; then
  gh release upload "$tag" "$assets"/* --repo "$repo" --clobber
else
  set -- --verify-tag --title "obelisk-k8s $version" --notes "$(scripts/release-notes.sh "$tag")"
  case "$version" in *-*) set -- "$@" --prerelease ;; esac
  gh release create "$tag" "$assets"/* --repo "$repo" "$@"
fi
