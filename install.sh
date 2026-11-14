#!/usr/bin/env bash
# Installs akshually from its GitHub release, without sudo:
#
#   curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash
#
# Installs the latest stable release, or the tag in `AKSHUALLY_VERSION` (pre-releases included), after
# checking the asset against the release's `SHA256SUMS`. Re-running it replaces the install in place.
#
#   Linux:  ~/.local/bin/akshually and ~/.local/share/applications/akshually.desktop
#   macOS:  ~/Applications/akshually.app, and a ~/.local/bin/akshually symlink to the binary inside it
#
# These locations are a contract: self-update replaces installs found there (docs/adr/0003).
# `AKSHUALLY_RELEASES_URL` points it at another copy of the Releases tree; the tests use a `file://` one.
#
# Written for bash 3.2, which macOS still ships. Everything runs from `main`, called on the last line,
# so a download cut short by `curl | bash` runs nothing.
set -euo pipefail

releases_page=https://github.com/jkoestinger/akshually/releases

say() { echo "akshually: $*"; }

die() {
    echo "akshually: $*" >&2
    exit 1
}

# Prints the SHA-256 of a file: Linux has `sha256sum`, macOS has `shasum`.
sha256() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

# Downloads `$1` to `$2`.
fetch() {
    curl -fsSL --retry 3 -o "$2" "$1" || die "can't download $1"
}

# Prints the version the binary at `$1` reports (`akshually 1.2.3`), or nothing when there's no
# working install there.
installed_version() {
    [[ -x $1 ]] || return 0
    "$1" --version 2>/dev/null || true
}

main() {
    local releases=${AKSHUALLY_RELEASES_URL:-$releases_page}
    local version=${AKSHUALLY_VERSION:-}
    local bin_dir="$HOME/.local/bin"
    local os arch target ext
    command -v curl >/dev/null || die "curl is needed to download akshually"

    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ext=tar.gz ;;
    Darwin/arm64) target=aarch64-apple-darwin ext=zip ;;
    Darwin/x86_64) target=x86_64-apple-darwin ext=zip ;;
    *) die "there's no akshually release for $os $arch; see $releases_page for the platforms there are" ;;
    esac

    # Global, for the `EXIT` trap.
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT

    # `SHA256SUMS` names every asset, so it also gives the latest stable release's tag without the
    # GitHub API: `releases/latest/download/` serves the newest release that isn't a pre-release.
    local sums_url
    if [[ -n $version ]]; then
        sums_url="$releases/download/$version/SHA256SUMS"
    else
        sums_url="$releases/latest/download/SHA256SUMS"
    fi
    fetch "$sums_url" "$work/SHA256SUMS"
    local asset expected tag
    asset=$(awk -v suffix="-$target.$ext" \
        'length($2) > length(suffix) && substr($2, length($2) - length(suffix) + 1) == suffix { print $2; exit }' \
        "$work/SHA256SUMS")
    [[ -n $asset ]] || die "the release has no asset for $target; see $releases_page"
    expected=$(awk -v name="$asset" '$2 == name { print $1; exit }' "$work/SHA256SUMS")
    tag=${asset#akshually-}
    tag=${tag%-"$target.$ext"}

    say "downloading $asset"
    fetch "$releases/download/$tag/$asset" "$work/$asset"
    [[ $(sha256 "$work/$asset") == "$expected" ]] ||
        die "$asset doesn't match its hash in SHA256SUMS, so nothing was installed; try again, or report it at $releases_page"

    mkdir "$work/unpacked"
    local old new
    old=$(installed_version "$bin_dir/akshually")
    if [[ $ext == tar.gz ]]; then
        tar -C "$work/unpacked" -xzf "$work/$asset"
        local dir="$work/unpacked/${asset%.tar.gz}"
        [[ -f $dir/akshually && -f $dir/akshually.desktop ]] ||
            die "$asset doesn't hold akshually and its launcher entry"
        install_linux "$dir" "$bin_dir"
    else
        unzip -q "$work/$asset" -d "$work/unpacked"
        [[ -f $work/unpacked/akshually.app/Contents/MacOS/akshually ]] || die "$asset doesn't hold akshually.app"
        install_macos "$work/unpacked/akshually.app" "$bin_dir"
    fi
    new=$("$bin_dir/akshually" --version) || die "the installed $bin_dir/akshually doesn't run"

    if [[ -n $old ]]; then
        say "replaced $old with $new"
    else
        say "installed $new"
    fi
    case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *)
        say "$bin_dir isn't on your PATH; add this line to your shell's startup file:"
        # Printed for the user to paste, so `$HOME` and `$PATH` stay unexpanded.
        # shellcheck disable=SC2016
        echo '  export PATH="$HOME/.local/bin:$PATH"'
        ;;
    esac
}

# Installs the binary and the launcher entry from the unpacked tarball dir `$1` into bin dir `$2`.
# Each file is written beside its target and renamed over it, so a running akshually keeps its
# binary and a failed copy leaves the old file whole.
install_linux() {
    local dir=$1 bin_dir=$2
    local apps="$HOME/.local/share/applications"
    mkdir -p "$bin_dir" "$apps"
    install -m 755 "$dir/akshually" "$bin_dir/.akshually.new"
    mv -f "$bin_dir/.akshually.new" "$bin_dir/akshually"
    install -m 644 "$dir/akshually.desktop" "$apps/.akshually.desktop.new"
    mv -f "$apps/.akshually.desktop.new" "$apps/akshually.desktop"
}

# Installs the unpacked bundle `$1` as `~/Applications/akshually.app` and links `$2/akshually` to the
# binary inside it, so the CLI and the app stay one binary.
install_macos() {
    local app=$1 bin_dir=$2
    local apps="$HOME/Applications"
    # curl sets no quarantine flag, but a proxy or a copied download could: clear it so Gatekeeper
    # doesn't call the unsigned app damaged.
    if command -v xattr >/dev/null; then
        xattr -dr com.apple.quarantine "$app" 2>/dev/null || true
    fi
    mkdir -p "$apps" "$bin_dir"
    # Copy beside the old bundle first (the temp dir may be on another volume), then swap it in.
    rm -rf "$apps/.akshually.app.new"
    cp -R "$app" "$apps/.akshually.app.new"
    rm -rf "$apps/akshually.app"
    mv "$apps/.akshually.app.new" "$apps/akshually.app"
    ln -sfn "$apps/akshually.app/Contents/MacOS/akshually" "$bin_dir/akshually"
}

main "$@"
