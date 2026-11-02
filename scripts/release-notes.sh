#!/bin/sh
# The release notes (markdown) for a version tag.
#
#   scripts/release-notes.sh v1.2.3
set -eu

tag="${1:?usage: scripts/release-notes.sh <tag>}"
version="${tag#v}"

cat <<MD
## obelisk-k8s $version

**Install** (Linux, macOS; needs a GitLab token that can read this project, in \`GITLAB_TOKEN\`):

\`\`\`sh
curl -fsSL -H "PRIVATE-TOKEN: \$GITLAB_TOKEN" "https://gitlab.krr-software-solutions.com/api/v4/projects/24/repository/files/install.sh/raw?ref=$tag" | OBELISK_K8S_VERSION=$version sh
\`\`\`

Or download the archive for your platform below.
MD
