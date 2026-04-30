#!/usr/bin/env bash
# Update the Scoop manifest for a given release tag.
#
# Usage: ./packaging/scoop/update-manifest.sh v0.1.0
# Writes to $OUTPUT (default: stdout).
set -euo pipefail

tag="${1:-}"
if [[ -z "$tag" ]]; then
  echo "usage: $0 <tag>   (e.g. v0.1.0)" >&2
  exit 1
fi
version="${tag#v}"

repo="gmilano/fast-grep-rust"
base="https://github.com/${repo}/releases/download/${tag}"
template="$(dirname "$0")/fast-grep.json"
output="${OUTPUT:-/dev/stdout}"

sha=$(curl -fsSL "${base}/fast-grep-${tag}-x86_64-pc-windows-msvc.zip.sha256" | tr -d '[:space:]')

sed \
  -e "s|\"version\": \"[^\"]*\"|\"version\": \"${version}\"|" \
  -e "s|/v0\.1\.0/|/v${version}/|g" \
  -e "s|fast-grep-v0\.1\.0-|fast-grep-v${version}-|g" \
  -e "s|REPLACE_WITH_X86_64_WINDOWS_SHA256|${sha}|" \
  "$template" > "$output"
