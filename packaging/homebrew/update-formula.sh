#!/usr/bin/env bash
# Update the Homebrew formula for a given release tag.
#
# Usage: ./packaging/homebrew/update-formula.sh v0.1.0
#
# Downloads the .sha256 sidecars from the GitHub release for each
# darwin/linux target, substitutes them and the version into the
# formula template, and writes the result to stdout (or to the path
# given by $OUTPUT, e.g. a checkout of homebrew-fast-grep/Formula/fast-grep.rb).
set -euo pipefail

tag="${1:-}"
if [[ -z "$tag" ]]; then
  echo "usage: $0 <tag>   (e.g. v0.1.0)" >&2
  exit 1
fi
version="${tag#v}"

repo="gmilano/fast-grep-rust"
base="https://github.com/${repo}/releases/download/${tag}"
template="$(dirname "$0")/fast-grep.rb"
output="${OUTPUT:-/dev/stdout}"

fetch_sha() {
  local target="$1"
  local url="${base}/fast-grep-${tag}-${target}.tar.gz.sha256"
  curl -fsSL "$url" | tr -d '[:space:]'
}

aarch64_darwin=$(fetch_sha aarch64-apple-darwin)
x86_64_darwin=$(fetch_sha x86_64-apple-darwin)
aarch64_linux=$(fetch_sha aarch64-unknown-linux-gnu)
x86_64_linux=$(fetch_sha x86_64-unknown-linux-gnu)

sed \
  -e "s|^  version \".*\"|  version \"${version}\"|" \
  -e "s|REPLACE_WITH_AARCH64_DARWIN_SHA256|${aarch64_darwin}|" \
  -e "s|REPLACE_WITH_X86_64_DARWIN_SHA256|${x86_64_darwin}|" \
  -e "s|REPLACE_WITH_AARCH64_LINUX_SHA256|${aarch64_linux}|" \
  -e "s|REPLACE_WITH_X86_64_LINUX_SHA256|${x86_64_linux}|" \
  "$template" > "$output"
