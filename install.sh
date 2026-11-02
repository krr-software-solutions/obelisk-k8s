#!/bin/sh
# Install obelisk-k8s from a GitLab release: download and verify the binary for this machine, and
# put it in ~/.local/bin (no root needed).
#
#   export GITLAB_TOKEN=glpat-…
#   curl -fsSL -H "PRIVATE-TOKEN: $GITLAB_TOKEN" \
#     "https://gitlab.krr-software-solutions.com/api/v4/projects/24/repository/files/install.sh/raw?ref=main" | sh
#
# The project is private, so every download needs a token that can read it: a personal (or project)
# access token with `read_api`, exported as GITLAB_TOKEN (so this script sees it too, not just curl).
# In CI, CI_JOB_TOKEN works too.
#
# Environment:
#   GITLAB_TOKEN                 the token (sent as PRIVATE-TOKEN)
#   OBELISK_K8S_VERSION          the version to install, e.g. 0.1.2 (default: the latest release that
#                                isn't a pre-release, like 1.2.3-rc.1)
#   OBELISK_K8S_TARGET           override the detected target, e.g. aarch64-unknown-linux-musl
#   OBELISK_K8S_INSTALL_DIR      where to put the binary (default: ~/.local/bin)
#
# Linux (x86_64, aarch64; static, so any distro) and macOS (x86_64, arm64). It runs `helm`,
# `kubectl` and (for local clusters) `k3d` and `docker`, which it doesn't install. To uninstall,
# delete the binary.
#
# Everything is inside main(), called on the last line, so a partial download runs nothing.
set -eu

API="https://gitlab.krr-software-solutions.com/api/v4/projects/24"

say() { printf 'obelisk-k8s-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
has() { command -v "$1" > /dev/null 2>&1; }

# The Rust target triple of the release archive for this machine.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported CPU architecture: $arch (prebuilt: x86_64, aarch64)" ;;
  esac
  case "$os" in
    Linux) echo "$arch-unknown-linux-musl" ;;
    Darwin)
      # An x86_64 shell under Rosetta still gets the native arm64 binary.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || echo 0)" = 1 ]; then
        arch=aarch64
      fi
      echo "$arch-apple-darwin"
      ;;
    *) die "unsupported OS: $os (prebuilt: Linux, macOS)" ;;
  esac
}

# The auth header for every request: a personal token, else CI's job token.
auth_header() {
  if [ -n "${GITLAB_TOKEN:-}" ]; then
    echo "PRIVATE-TOKEN: $GITLAB_TOKEN"
  elif [ -n "${CI_JOB_TOKEN:-}" ]; then
    echo "JOB-TOKEN: $CI_JOB_TOKEN"
  else
    die "set GITLAB_TOKEN to a token that can read the project (read_api): it's private"
  fi
}

download() { # <url> <file>
  if has curl; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
      --header "$header" --output "$2" "$1"
  elif has wget; then
    wget --quiet --header="$header" --output-document="$2" "$1"
  else
    die "need curl or wget"
  fi
}

sha256_of() { # <file>
  if has sha256sum; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif has shasum; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

main() {
  has curl || has wget || die "need curl or wget"
  header="$(auth_header)"
  target="${OBELISK_K8S_TARGET:-$(detect_target)}"
  version="${OBELISK_K8S_VERSION:-}"
  version="${version#v}"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  if [ -z "$version" ]; then
    # Newest first; the first that isn't a pre-release.
    download "$API/releases?per_page=100" "$tmp/releases.json" ||
      die "couldn't list the releases (does the token have read_api on the project?)"
    tag="$(grep -o '"tag_name":"[^"]*"' "$tmp/releases.json" | cut -d '"' -f 4 | grep -v -- - | head -n 1 || true)"
    [ -n "$tag" ] || die "no release yet (other than pre-releases): set OBELISK_K8S_VERSION"
    version="${tag#v}"
  fi

  archive="obelisk-k8s-$version-$target.tar.gz"
  base="$API/packages/generic/obelisk-k8s/$version"
  say "downloading $archive"
  download "$base/$archive" "$tmp/$archive" || die "couldn't download $base/$archive"
  download "$base/$archive.sha256" "$tmp/$archive.sha256" || die "couldn't download $base/$archive.sha256"

  expected="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
  actual="$(sha256_of "$tmp/$archive")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $archive: expected $expected, got $actual"

  tar -xzf "$tmp/$archive" -C "$tmp"
  binary="$(find "$tmp" -type f -name obelisk-k8s | head -n 1)"
  [ -n "$binary" ] || die "no obelisk-k8s binary in $archive"
  chmod 755 "$binary"

  dir="${OBELISK_K8S_INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$dir" || die "can't create $dir"
  # Copy then rename, so a running obelisk-k8s is never left half-written.
  cp "$binary" "$dir/.obelisk-k8s.tmp"
  mv -f "$dir/.obelisk-k8s.tmp" "$dir/obelisk-k8s"
  say "installed $("$dir/obelisk-k8s" --version) to $dir/obelisk-k8s"
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "note: $dir isn't on your PATH" ;;
  esac
}

main "$@"
