#!/usr/bin/env bash
#
# Tag a release, push it, and stamp the Homebrew formula with the new
# version + tarball sha256.
#
# Usage:  packaging/release.sh <version>      e.g.  packaging/release.sh 0.1.0
#
# After this succeeds, copy packaging/ccal.rb into your tap repo
# (github.com/hillman/homebrew-tap) at Formula/ccal.rb and push it.
# Users then install with:  brew install hillman/tap/ccal
#
set -euo pipefail

REPO="hillman/ccal"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FORMULA="$SCRIPT_DIR/ccal.rb"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <version>   (e.g. $0 0.1.0)" >&2
  exit 1
fi

VERSION="${1#v}"          # accept "0.1.0" or "v0.1.0"
TAG="v$VERSION"
TARBALL="https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz"

# Sanity: Cargo.toml version should match the tag.
CARGO_VERSION="$(grep -m1 '^version' "$SCRIPT_DIR/../Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"
if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
  echo "WARNING: Cargo.toml version is $CARGO_VERSION but you are tagging $VERSION." >&2
  read -rp "Continue anyway? [y/N] " ans
  [[ "$ans" == "y" || "$ans" == "Y" ]] || exit 1
fi

# Create and push the tag (skip if it already exists locally).
if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "Tag $TAG already exists locally; skipping creation."
else
  git tag -a "$TAG" -m "Release $TAG"
fi
git push origin "$TAG"

# GitHub generates the source tarball lazily; poll until it is available.
echo "Waiting for GitHub to publish $TARBALL ..."
for i in $(seq 1 30); do
  if curl -fsIL "$TARBALL" >/dev/null 2>&1; then
    break
  fi
  sleep 2
  if [[ $i -eq 30 ]]; then
    echo "Timed out waiting for the release tarball." >&2
    exit 1
  fi
done

SHA="$(curl -fsSL "$TARBALL" | shasum -a 256 | awk '{print $1}')"
echo "tarball sha256: $SHA"

# Stamp url + sha256 into the formula (BSD sed, macOS).
sed -i '' -E \
  -e "s|refs/tags/v[0-9][^\"]*\.tar\.gz|refs/tags/$TAG.tar.gz|" \
  -e "s|sha256 \"[0-9a-f]*\"|sha256 \"$SHA\"|" \
  -e "s|sha256 \"REPLACE_WITH_TARBALL_SHA256\"|sha256 \"$SHA\"|" \
  "$FORMULA"

echo
echo "Updated $FORMULA:"
grep -E '  (url|sha256) ' "$FORMULA"
echo
echo "Next: copy this into your tap repo and push:"
echo "  cp $FORMULA <homebrew-tap>/Formula/ccal.rb"
echo "  (cd <homebrew-tap> && git add Formula/ccal.rb && git commit -m '$REPO $TAG' && git push)"
