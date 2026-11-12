#!/usr/bin/env bash
# Packages a built `akshually` binary into a release asset, and prints the asset's path.
#
#   packaging/package.sh linux <binary> <asset-stem> <out-dir>   -> <asset-stem>.tar.gz
#   packaging/package.sh macos <binary> <asset-stem> <out-dir>   -> <asset-stem>.zip (akshually.app)
#
# Runs `<binary> --version` first, as the smoke test, and takes the bundle version from it.
# The release workflow calls this; run it locally to check an asset by hand.
set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 linux|macos <binary> <asset-stem> <out-dir>" >&2
    exit 2
fi
platform=$1 binary=$2 stem=$3 out=$4
root=$(cd "$(dirname "$0")/.." && pwd)

version_line=$("$binary" --version)
echo "$version_line" >&2
version=${version_line#akshually }

mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

case $platform in
linux)
    dir="$work/$stem"
    mkdir -p "$dir"
    install -m 755 "$binary" "$dir/akshually"
    install -m 644 "$root/packaging/linux/akshually.desktop" "$root/README.md" \
        "$root/LICENSE-APACHE" "$root/LICENSE-MIT" "$dir/"
    asset="$out/$stem.tar.gz"
    tar -C "$work" -czf "$asset" "$stem"
    ;;
macos)
    app="$work/akshually.app/Contents"
    mkdir -p "$app/MacOS" "$app/Resources"
    install -m 755 "$binary" "$app/MacOS/akshually"
    sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" >"$app/Info.plist"
    install -m 644 "$root/LICENSE-APACHE" "$root/LICENSE-MIT" "$app/Resources/"
    # Ad-hoc signature (no Developer ID): seals the bundle so Gatekeeper sees one consistent app.
    if command -v codesign >/dev/null; then
        codesign --force --sign - "$work/akshually.app"
    fi
    asset="$out/$stem.zip"
    rm -f "$asset"
    if command -v ditto >/dev/null; then
        ditto -c -k --keepParent "$work/akshually.app" "$asset"
    else
        (cd "$work" && zip -qry "$asset" akshually.app)
    fi
    ;;
*)
    echo "unknown platform: $platform" >&2
    exit 2
    ;;
esac

echo "$asset"
