#!/bin/bash
set -e

set -e
level=${1:-patch}
echo "Releasing.."
echo "=== cargo check ==="
cargo check
echo "=== cargo clippy ==="
cargo clippy -- -D warnings
echo "=== cargo bump ${level} ==="
cargo bump ${level}
echo "=== cargo check ==="
cargo check
echo "=== create release message"
new_version=$(grep -E '^version' Cargo.toml | cut -d'"' -f2)
echo "# ${new_version}" > .latest_release.txt
git log $(git describe --tags --abbrev=0)..HEAD --pretty=format:%B >> .latest_release.txt
vim .latest_release.txt
git add .latest_release.txt
echo "=== git commit ==="
git commit -am "Release ${new_version}"
echo "=== Tagging version v${new_version} ==="
git tag v${new_version}
echo "=== git push ==="
git push && git push --tags
cargo publish
