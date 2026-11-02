#!/bin/sh
# The release notes (markdown) for a version tag, shared by the GitLab and GitHub releases.
#
#   scripts/release-notes.sh v1.2.3
set -eu

tag="${1:?usage: scripts/release-notes.sh <tag>}"
version="${tag#v}"

cat <<MD
## obelisk-k8s $version

**Install** (Linux, macOS; the binary goes in \`~/.local/bin\`):

\`\`\`sh
curl -fsSL https://github.com/krr-software-solutions/obelisk-k8s/releases/download/$tag/install.sh | sh
\`\`\`

Or download the archive for your platform below.
MD
