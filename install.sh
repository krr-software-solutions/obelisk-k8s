#!/bin/sh
# Install obelisk-k8s from a GitHub release: download and verify the binary for this machine, put
# it in ~/.local/bin, and install its shell completions for you (no root needed).
#
#   curl -fsSL https://github.com/krr-software-solutions/obelisk-k8s/releases/latest/download/install.sh | sh
#
# Environment:
#   OBELISK_K8S_VERSION          the version to install, e.g. 0.1.2 (default: the latest release; GitHub
#                                never counts a pre-release, like 1.2.3-rc.1, as the latest)
#   OBELISK_K8S_TARGET           override the detected target, e.g. aarch64-unknown-linux-musl
#   OBELISK_K8S_INSTALL_DIR      where to put the binary (default: ~/.local/bin)
#   OBELISK_K8S_NO_COMPLETIONS   set to skip the shell completions
#
# Completions go where each installed shell loads a user's from: bash (bash-completion 2)
# ~/.local/share/bash-completion/completions, fish ~/.config/fish/completions, zsh
# ~/.local/share/zsh/site-functions (which zsh only reads if it's on your fpath: see the note).
#
# Linux (x86_64, aarch64; static, so any distro) and macOS (x86_64, arm64). It runs `helm`,
# `kubectl` and (for local clusters) `k3d` and `docker`, which it doesn't install. To uninstall,
# delete the binary and the three completion files.
#
# Everything is inside main(), called on the last line, so a partial download runs nothing.
set -eu

REPO="krr-software-solutions/obelisk-k8s"

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

download() { # <url> <file>
  if has curl; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --output "$2" "$1"
  elif has wget; then
    wget --quiet --output-document="$2" "$1"
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

# Writes completions for each installed shell, with the binary at <path>.
install_completions() { # <path>
  data="${XDG_DATA_HOME:-$HOME/.local/share}"
  config="${XDG_CONFIG_HOME:-$HOME/.config}"
  installed=""
  for shell in bash zsh fish; do
    has "$shell" || continue
    case "$shell" in
      bash) file="$data/bash-completion/completions/obelisk-k8s" ;;
      zsh) file="$data/zsh/site-functions/_obelisk-k8s" ;;
      fish) file="$config/fish/completions/obelisk-k8s.fish" ;;
    esac
    if ! { mkdir -p "$(dirname "$file")" && "$1" completions "$shell" > "$file"; }; then
      die "couldn't write the $shell completions to $file"
    fi
    installed="$installed $shell"
  done
  if [ -n "$installed" ]; then
    say "installed shell completions for:$installed"
  fi
  case "$installed" in
    *zsh*) say "note: for zsh, put $data/zsh/site-functions on your fpath before compinit, e.g. in ~/.zshrc: fpath=($data/zsh/site-functions \$fpath)" ;;
  esac
}

main() {
  has curl || has wget || die "need curl or wget"
  target="${OBELISK_K8S_TARGET:-$(detect_target)}"
  version="${OBELISK_K8S_VERSION:-}"
  version="${version#v}"

  if [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/v$version"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  archive="obelisk-k8s-$target.tar.gz"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  say "downloading $archive (${version:-latest})"
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
  if [ -z "${OBELISK_K8S_NO_COMPLETIONS:-}" ]; then
    install_completions "$dir/obelisk-k8s"
  fi
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "note: $dir isn't on your PATH" ;;
  esac
}

main "$@"
