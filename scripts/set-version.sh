#!/bin/sh
# Stamp the release version into the workspace. The committed Cargo.toml carries the placeholder
# `0.0.0` (the workspace version, and the exact `=0.0.0` pins between the workspace crates); this
# rewrites all of them, then refreshes Cargo.lock to match.
#
#   scripts/set-version.sh v1.2.3      # a leading `v` is stripped (git tags are `v1.2.3`)
#   scripts/set-version.sh 1.2.3-rc.1
set -eu

version="${1:?usage: scripts/set-version.sh <version|vVERSION>}"
version="${version#v}"

# Semver: MAJOR.MINOR.PATCH, optionally -PRERELEASE and/or +BUILD.
if ! printf '%s\n' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
  echo "set-version: '$version' is not a semver version" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

placeholders="$(grep -c '"=\{0,1\}0\.0\.0"' Cargo.toml || true)"
if [ "$placeholders" -eq 0 ]; then
  echo "set-version: no 0.0.0 placeholder left in Cargo.toml (already stamped?)" >&2
  exit 1
fi

sed -i.bak -e "s/\"0\.0\.0\"/\"$version\"/g" -e "s/\"=0\.0\.0\"/\"=$version\"/g" Cargo.toml
rm -f Cargo.toml.bak
cargo update --workspace --quiet

echo "set-version: $version ($placeholders placeholders)"
