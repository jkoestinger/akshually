#!/usr/bin/env bash
# Installs akshually from its GitHub release, without sudo:
#
#   curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash
#
# Installs the latest stable release, or the tag in `AKSHUALLY_VERSION` (pre-releases included), after
# checking the asset against the release's `SHA256SUMS`. Re-running it replaces the install in place.
#
#   Linux:  ~/.local/bin/akshually, and under the data home (`$XDG_DATA_HOME`, else ~/.local/share) the
#           launcher entry applications/akshually.desktop and the icon in icons/hicolor/
#   macOS:  ~/Applications/Akshually.app, and a ~/.local/bin/akshually symlink to the binary inside it
#
# These locations are a contract: self-update replaces installs found there (docs/adr/0003).
# `AKSHUALLY_RELEASES_URL` points it at another copy of the Releases tree, for the tests' `file://` one.
#
# Written for bash 3.2, which macOS still ships. Everything runs from `main`, called on the last line,
# so a download cut short by `curl | bash` runs nothing.
set -euo pipefail

releases_page=https://github.com/jkoestinger/akshually/releases
# How often curl retries a download that fails for a transient reason (a timeout, a 5xx), since
# GitHub's release CDN sometimes drops a connection. A 404 isn't retried.
download_retries=3

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

# Downloads `$1` to `$2`. Returns non-zero when it can't, for the caller to explain.
fetch() {
    curl -fsSL --retry "$download_retries" -o "$2" "$1"
}

# Prints the version the binary at `$1` reports (`akshually 1.2.3`), or nothing when there's no
# working install there.
installed_version() {
    [[ -x $1 ]] || return 0
    "$1" --version 2>/dev/null || true
}

main() {
    local releases_url=${AKSHUALLY_RELEASES_URL:-$releases_page}
    local version=${AKSHUALLY_VERSION:-}
    local bin_dir="$HOME/.local/bin"
    local platform target ext
    command -v curl >/dev/null || die "curl is needed to download akshually; install it and run this again"

    # Assigned on its own line so that `set -e` stops here when `release_target` dies.
    platform=$(release_target)
    read -r target ext <<<"$platform"

    # Global, for the `EXIT` trap.
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT

    # `SHA256SUMS` names every asset, so it also gives the latest stable release's tag without the
    # GitHub API: `releases/latest/download/` serves the newest release that isn't a pre-release.
    local sums_url release
    if [[ -n $version ]]; then
        sums_url="$releases_url/download/$version/SHA256SUMS" release="release $version"
    else
        sums_url="$releases_url/latest/download/SHA256SUMS" release="the latest release"
    fi
    fetch "$sums_url" "$work/SHA256SUMS" ||
        die "can't download SHA256SUMS for $release from $sums_url; releases from before the install script have none, so set AKSHUALLY_VERSION to a newer tag from $releases_page"
    local asset expected tag
    asset=$(awk -v suffix="-$target.$ext" \
        'length($2) > length(suffix) && substr($2, length($2) - length(suffix) + 1) == suffix { print $2; exit }' \
        "$work/SHA256SUMS")
    [[ -n $asset ]] || die "$release has no asset for $target; pick another from $releases_page"
    expected=$(awk -v name="$asset" '$2 == name { print $1; exit }' "$work/SHA256SUMS")
    if [[ -n $version ]]; then
        tag=$version
    else
        tag=${asset#akshually-}
        tag=${tag%-"$target.$ext"}
    fi

    say "downloading $asset"
    fetch "$releases_url/download/$tag/$asset" "$work/$asset" ||
        die "can't download $asset from $releases_url/download/$tag; check your connection and run this again"
    [[ $(sha256 "$work/$asset") == "$expected" ]] ||
        die "$asset doesn't match its hash in SHA256SUMS, so nothing was installed; try again, or report it at $releases_page"

    mkdir "$work/unpacked"
    local old new
    old=$(installed_version "$bin_dir/akshually")
    if [[ $ext == tar.gz ]]; then
        tar -C "$work/unpacked" -xzf "$work/$asset"
        local dir="$work/unpacked/${asset%.tar.gz}"
        # Releases from before the icon hold none, so it is installed when the asset has it.
        [[ -f $dir/akshually && -f $dir/akshually.desktop ]] ||
            die "$asset doesn't hold akshually and its launcher entry, so nothing was installed; report it at $releases_page"
        install_linux "$dir" "$bin_dir"
    else
        unzip -q "$work/$asset" -d "$work/unpacked"
        # Releases from before the rename hold `akshually.app`; either way it is installed as `Akshually.app`.
        local app='' name
        for name in Akshually.app akshually.app; do
            if [[ -f $work/unpacked/$name/Contents/MacOS/akshually ]]; then
                app="$work/unpacked/$name"
                break
            fi
        done
        [[ -n $app ]] ||
            die "$asset doesn't hold Akshually.app, so nothing was installed; report it at $releases_page"
        install_macos "$app" "$bin_dir"
    fi
    new=$("$bin_dir/akshually" --version) ||
        die "the installed $bin_dir/akshually doesn't run; report it at $releases_page with your OS version"

    if [[ -n $old ]]; then
        say "replaced $old with $new"
    else
        say "installed $new"
    fi
    path_hint "$bin_dir"
}

# Prints the target triple and archive extension of this machine's release asset, or dies when
# there's no release for it.
release_target() {
    local os arch
    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
    Linux/x86_64) echo x86_64-unknown-linux-gnu tar.gz ;;
    Darwin/arm64) echo aarch64-apple-darwin zip ;;
    Darwin/x86_64) echo x86_64-apple-darwin zip ;;
    *) die "there's no akshually release for $os $arch; see $releases_page for the platforms there are" ;;
    esac
}

# Tells the user how to reach the install at `$1/akshually` when their `PATH` doesn't: the line to
# add when `$1` isn't on it, or a warning when another `akshually` comes first, such as a
# `/usr/local/bin` link left by the README's old macOS steps.
path_hint() {
    local bin_dir=$1 found
    case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *)
        say "$bin_dir isn't on your PATH; add this line to your shell's startup file:"
        # Printed for the user to paste, so `$HOME` and `$PATH` stay unexpanded.
        # shellcheck disable=SC2016
        echo '  export PATH="$HOME/.local/bin:$PATH"'
        return
        ;;
    esac
    found=$(command -v akshually || true)
    if [[ -n $found && $found != "$bin_dir/akshually" ]]; then
        say "\`akshually\` runs $found, not this install; remove it, or put $bin_dir before $(dirname "$found") on your PATH"
    fi
}

# Prints the user's XDG data home. The app repairs its launcher entry there at launch, so both must
# agree where it is; a relative `$XDG_DATA_HOME` is ignored, as the spec says.
data_home() {
    case ${XDG_DATA_HOME:-} in
    /*) echo "$XDG_DATA_HOME" ;;
    *) echo "$HOME/.local/share" ;;
    esac
}

# Installs the binary, the launcher entry and the icon from the unpacked tarball dir `$1` into bin dir `$2`.
install_linux() {
    local dir=$1 bin_dir=$2
    local data icon
    data=$(data_home)
    replace_file 755 "$dir/akshually" "$bin_dir/akshually"
    replace_file 644 "$dir/akshually.desktop" "$data/applications/akshually.desktop"
    [[ -d $dir/icons/hicolor ]] || return 0
    while read -r icon; do
        replace_file 644 "$dir/icons/hicolor/$icon" "$data/icons/hicolor/$icon"
    done < <(cd "$dir/icons/hicolor" && find . -type f -name akshually.png | sed 's|^\./||')
}

# Copies `$2` to `$3` with mode `$1`. It writes beside `$3` and renames over it, so a running
# akshually keeps its binary and a failed copy leaves the old file whole.
replace_file() {
    local mode=$1 src=$2 dest=$3
    local tmp
    tmp="$(dirname "$dest")/.$(basename "$dest").new"
    mkdir -p "$(dirname "$dest")"
    install -m "$mode" "$src" "$tmp"
    mv -f "$tmp" "$dest"
}

# Installs the unpacked bundle `$1` as `~/Applications/Akshually.app` and links `$2/akshually` to the
# binary inside it, so the CLI and the app stay one binary.
install_macos() {
    local app=$1 bin_dir=$2
    local apps="$HOME/Applications"
    mkdir -p "$apps" "$bin_dir"
    # Copy beside the old bundle first (the temp dir may be on another volume), then swap it in. Both
    # names go before the move: on a case-insensitive volume they are one directory, and removing
    # the old name after the move would remove the new bundle.
    rm -rf "$apps/.Akshually.app.new"
    cp -R "$app" "$apps/.Akshually.app.new"
    rm -rf "$apps/akshually.app" "$apps/Akshually.app"
    mv "$apps/.Akshually.app.new" "$apps/Akshually.app"
    # curl sets no quarantine flag, but a proxy or a copied download could: clear it so Gatekeeper
    # doesn't call the unsigned app damaged.
    if command -v xattr >/dev/null; then
        xattr -dr com.apple.quarantine "$apps/Akshually.app" 2>/dev/null || true
    fi
    ln -sfn "$apps/Akshually.app/Contents/MacOS/akshually" "$bin_dir/akshually"
}

main "$@"
