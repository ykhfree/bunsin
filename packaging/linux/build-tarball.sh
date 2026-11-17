#!/usr/bin/env bash
# Builds the Linux server package: a tarball with bunsind, the systemd unit, the sysusers file,
# the environment template, INSTALL.md, LICENSE and README.md.
#
# Usage: packaging/linux/build-tarball.sh [output directory]
# The default output directory is target/package. The script prints the path of the tarball.
# It holds no secret, and it reads none.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out=${1:-"$repo/target/package"}
target_dir=${CARGO_TARGET_DIR:-"$repo/target"}
here="$repo/packaging/linux"

cd "$repo"
# `cargo pkgid` prints for example path+file:///src/bunsin/crates/daemon#bunsin-daemon@0.1.0.
version=$(cargo pkgid -p bunsin-daemon --locked)
version=${version##*[@#]}
host=$(rustc -vV | sed -n 's/^host: //p')
name="bunsin-server-$version-$host"

cargo build --release -p bunsin-daemon --locked

stage="$out/$name"
rm -rf "$stage" "$out/$name.tar.gz" "$out/$name.tar.gz.sha256"
mkdir -p "$stage"
install -m 0755 "$target_dir/release/bunsind" "$stage/bunsind"
install -m 0644 "$here/bunsind.service" "$stage/bunsind.service"
install -m 0644 "$here/bunsin.sysusers" "$stage/bunsin.sysusers"
install -m 0644 "$here/bunsind.env.example" "$stage/bunsind.env.example"
install -m 0644 "$here/INSTALL.md" "$stage/INSTALL.md"
install -m 0644 "$repo/LICENSE" "$stage/LICENSE"
install -m 0644 "$repo/README.md" "$stage/README.md"

tar -C "$out" -czf "$out/$name.tar.gz" "$name"
rm -rf "$stage"
(
    cd "$out"
    if command -v sha256sum >/dev/null; then
        sha256sum "$name.tar.gz" >"$name.tar.gz.sha256"
    else
        shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256"
    fi
)
echo "$out/$name.tar.gz"
