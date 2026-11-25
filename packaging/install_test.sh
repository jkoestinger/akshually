#!/usr/bin/env bash
# Seam D: `install.sh` run as a process, against a `file://` fixture of release assets, into a
# throwaway `HOME`, with a fake `uname` on `PATH` to pick the platform. Asserts on exit codes, output
# and the files left under `HOME`. Never touches the network or the real `HOME`.
#
# Also exercises `packaging/package.sh` as a process (the `package_*` tests), asserting on the
# contents of the assets it makes.
#
#   packaging/install_test.sh            runs every test
#   packaging/install_test.sh <name>…    runs the named tests
#
# The releases hold stand-in binaries, except in `installs_a_real_build`, which packages the binary
# at `AKSHUALLY_BINARY` (and is skipped without it).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

failures=0

# Fails the running test. Each test runs in its own subshell, so this ends only that test.
fail() {
    echo "    $*" >&2
    exit 1
}

# Writes a stand-in `akshually` binary that reports `version`.
fake_binary() {
    local path=$1 version=$2
    mkdir -p "$(dirname "$path")"
    printf '#!/bin/sh\necho "akshually %s"\n' "$version" >"$path"
    chmod 755 "$path"
}

# Publishes release `tag` (all three assets and `SHA256SUMS`) into the fixture at `$releases`, laid
# out as GitHub serves it: `download/<tag>/<asset>`, and `latest/download/<asset>` when the second
# argument is `latest`. The assets hold a stand-in binary, or a copy of the binary at `$3`.
publish() {
    local tag=$1 latest=${2:-} binary=${3:-}
    local version=${tag#v}
    local dir="$releases/download/$tag" build="$work/build/$tag"
    mkdir -p "$dir" "$build/bin"

    if [[ -n $binary ]]; then
        install -m 755 "$binary" "$build/bin/akshually"
    else
        fake_binary "$build/bin/akshually" "$version"
    fi
    "$root/packaging/package.sh" linux "$build/bin/akshually" "akshually-$tag-x86_64-unknown-linux-gnu" "$dir" \
        >/dev/null 2>&1
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
        "$root/packaging/package.sh" macos "$build/bin/akshually" "akshually-$tag-$target" "$dir" >/dev/null 2>&1
    done
    (cd "$dir" && sha256 akshually-* >SHA256SUMS)

    if [[ $latest == latest ]]; then
        rm -rf "$releases/latest"
        mkdir -p "$releases/latest"
        cp -R "$dir" "$releases/latest/download"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# Puts a fake `uname` first on `PATH` that reports `os` for `-s` and `arch` for `-m`.
fake_platform() {
    local os=$1 arch=$2
    mkdir -p "$work/fakebin"
    cat >"$work/fakebin/uname" <<EOF
#!/bin/sh
case \$1 in -s) echo $os ;; -m) echo $arch ;; *) echo $os ;; esac
EOF
    chmod 755 "$work/fakebin/uname"
}

# Runs `install.sh` with the test environment plus any `VAR=value` arguments. Leaves its exit code in
# `status` and its combined output in `output`. Runs it with `/bin/bash`, as `curl | bash` mostly
# does: on macOS that is bash 3.2.
run_install() {
    set +e
    output=$(env -u XDG_DATA_HOME HOME="$home" PATH="$work/fakebin:$PATH" AKSHUALLY_RELEASES_URL="file://$releases" "$@" \
        /bin/bash "$root/install.sh" 2>&1)
    status=$?
    set -e
}

test_installs_a_pinned_version_on_linux() {
    fake_platform Linux x86_64
    publish v1.2.3

    run_install AKSHUALLY_VERSION=v1.2.3

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $("$home/.local/bin/akshually" --version) == "akshually 1.2.3" ]] || fail "wrong binary installed"
    [[ -f $home/.local/share/applications/akshually.desktop ]] || fail "no launcher entry"
    [[ $output == *"akshually 1.2.3"* ]] || fail "doesn't report the installed version: $output"
}

test_installs_the_latest_stable_release_by_default() {
    fake_platform Linux x86_64
    publish v1.2.3 latest
    publish v1.3.0-rc.1

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $("$home/.local/bin/akshually" --version) == "akshually 1.2.3" ]] || fail "didn't install the latest stable"
}

test_installs_a_pinned_pre_release() {
    fake_platform Linux x86_64
    publish v1.2.3 latest
    publish v1.3.0-rc.1

    run_install AKSHUALLY_VERSION=v1.3.0-rc.1

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $("$home/.local/bin/akshually" --version) == "akshually 1.3.0-rc.1" ]] || fail "didn't install the pre-release"
}

test_refuses_an_asset_whose_hash_doesnt_match() {
    fake_platform Linux x86_64
    publish v1.2.3
    echo tampered >>"$releases/download/v1.2.3/akshually-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"

    run_install AKSHUALLY_VERSION=v1.2.3

    [[ $status -ne 0 ]] || fail "install succeeded with a tampered asset"
    [[ $output == *"SHA256SUMS"* ]] || fail "doesn't say the checksum failed: $output"
    [[ ! -e $home/.local ]] || fail "installed something: $(find "$home/.local")"
}

test_an_unsupported_platform_is_refused_with_the_releases_page() {
    fake_platform Linux aarch64
    publish v1.2.3 latest

    run_install

    [[ $status -ne 0 ]] || fail "install succeeded on Linux aarch64"
    [[ $output == *"https://github.com/jkoestinger/akshually/releases"* ]] || fail "no Releases page: $output"
    [[ ! -e $home/.local ]] || fail "installed something: $(find "$home/.local")"
}

# Checks a macOS install of `version` into `home`: the bundle, and the symlink into it.
assert_macos_install() {
    local version=$1
    local app="$home/Applications/Akshually.app" link="$home/.local/bin/akshually"
    [[ -x $app/Contents/MacOS/akshually ]] || fail "no app bundle"
    [[ -L $link ]] || fail "$link isn't a symlink"
    [[ $(readlink "$link") == "$app/Contents/MacOS/akshually" ]] || fail "$link points at $(readlink "$link")"
    [[ $("$link" --version) == "akshually $version" ]] || fail "wrong binary installed"
}

test_installs_the_app_and_a_symlink_on_apple_silicon() {
    fake_platform Darwin arm64
    publish v1.2.3 latest

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_macos_install 1.2.3
    [[ $output == *"akshually 1.2.3"* ]] || fail "doesn't report the installed version: $output"
}

test_installs_the_intel_asset_on_an_intel_mac() {
    fake_platform Darwin x86_64
    publish v1.2.3 latest
    rm "$releases/download/v1.2.3/akshually-v1.2.3-aarch64-apple-darwin.zip"

    run_install AKSHUALLY_VERSION=v1.2.3

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_macos_install 1.2.3
}

test_rerunning_reports_the_replaced_and_the_installed_version() {
    fake_platform Linux x86_64
    publish v1.2.3
    publish v1.3.0 latest
    run_install AKSHUALLY_VERSION=v1.2.3

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $output == *"replaced akshually 1.2.3 with akshually 1.3.0"* ]] || fail "doesn't report both versions: $output"
}

test_rerunning_on_linux_upgrades_the_install_in_place() {
    fake_platform Linux x86_64
    publish v1.2.3
    publish v1.3.0 latest
    run_install AKSHUALLY_VERSION=v1.2.3

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $("$home/.local/bin/akshually" --version) == "akshually 1.3.0" ]] || fail "didn't replace the binary"
    [[ $(ls -A "$home/.local/bin") == akshually ]] || fail "left extra files: $(ls -A "$home/.local/bin")"
}

test_rerunning_on_macos_upgrades_the_app_in_place() {
    fake_platform Darwin arm64
    publish v1.2.3
    publish v1.3.0 latest
    run_install AKSHUALLY_VERSION=v1.2.3

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_macos_install 1.3.0
    [[ $(ls -A "$home/Applications") == Akshually.app ]] || fail "left extra bundles: $(ls -A "$home/Applications")"
}

test_prints_the_path_line_when_the_bin_dir_isnt_on_path() {
    fake_platform Linux x86_64
    publish v1.2.3 latest

    run_install

    # The line the script prints for the user to paste, unexpanded.
    # shellcheck disable=SC2016
    [[ $output == *'export PATH="$HOME/.local/bin:$PATH"'* ]] || fail "no PATH line: $output"
    [[ ! -e $home/.bashrc && ! -e $home/.zshrc && ! -e $home/.profile ]] || fail "edited a shell rc file"
}

test_prints_no_path_line_when_the_bin_dir_is_on_path() {
    fake_platform Linux x86_64
    publish v1.2.3 latest

    run_install PATH="$home/.local/bin:$work/fakebin:$PATH"

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $output != *"export PATH"* ]] || fail "printed a PATH line: $output"
    [[ $output != *"not this install"* ]] || fail "warned about another akshually: $output"
}

test_warns_when_another_akshually_comes_first_on_path() {
    fake_platform Darwin arm64
    publish v1.2.3 latest
    fake_binary "$work/usr-local-bin/akshually" 0.0.1

    run_install PATH="$work/usr-local-bin:$home/.local/bin:$work/fakebin:$PATH"

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $output == *"runs $work/usr-local-bin/akshually, not this install"* ]] || fail "no warning: $output"
}

test_explains_a_release_without_sha256sums() {
    fake_platform Linux x86_64
    publish v1.2.3
    rm "$releases/download/v1.2.3/SHA256SUMS"

    run_install AKSHUALLY_VERSION=v1.2.3

    [[ $status -ne 0 ]] || fail "install succeeded without SHA256SUMS"
    [[ $output == *"SHA256SUMS for release v1.2.3"*"AKSHUALLY_VERSION to a newer tag"* ]] || fail "unclear: $output"
    [[ ! -e $home/.local ]] || fail "installed something: $(find "$home/.local")"
}

# Checks that the data home `$1` holds the launcher entry naming the icon, and every icon size
# `packaging/` ships, byte for byte.
assert_launcher_and_icons() {
    local data=$1 icon size
    grep -qx 'Icon=akshually' "$data/applications/akshually.desktop" || fail "the launcher entry names no icon"
    grep -qx 'Name=Akshually' "$data/applications/akshually.desktop" || fail "the launcher entry isn't named Akshually"
    for icon in "$root"/packaging/icons/hicolor/*/apps/akshually.png; do
        size=$(basename "$(dirname "$(dirname "$icon")")")
        cmp -s "$icon" "$data/icons/hicolor/$size/apps/akshually.png" || fail "the $size icon isn't installed"
    done
}

test_installs_the_icon_into_the_hicolor_theme_on_linux() {
    fake_platform Linux x86_64
    publish v1.2.3 latest

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_launcher_and_icons "$home/.local/share"
}

test_installs_the_launcher_entry_and_icon_under_xdg_data_home() {
    fake_platform Linux x86_64
    publish v1.2.3 latest

    run_install XDG_DATA_HOME="$home/xdg-data"

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_launcher_and_icons "$home/xdg-data"
    [[ ! -e $home/.local/share ]] || fail "wrote under the default data home too"
}

# Replaces the release `v1.2.3` with one as made before the icon: a tarball (or a zip) holding the
# old launcher entry, no icons and, on macOS, `akshually.app`.
publish_before_the_icon() {
    local platform=$1 dir="$releases/download/v1.2.3" old="$work/old-release"
    rm -rf "$old" "$dir" && mkdir -p "$old" "$dir"
    if [[ $platform == linux ]]; then
        local stem=akshually-v1.2.3-x86_64-unknown-linux-gnu
        mkdir "$old/$stem"
        fake_binary "$old/$stem/akshually" 1.2.3
        printf '[Desktop Entry]\nName=akshually\n' >"$old/$stem/akshually.desktop"
        tar -C "$old" -czf "$dir/$stem.tar.gz" "$stem"
    else
        mkdir -p "$old/akshually.app/Contents/MacOS"
        fake_binary "$old/akshually.app/Contents/MacOS/akshually" 1.2.3
        (cd "$old" && python3 -m zipfile -c "$dir/akshually-v1.2.3-aarch64-apple-darwin.zip" akshually.app)
    fi
    (cd "$dir" && sha256 akshually-* >SHA256SUMS)
    rm -rf "$releases/latest" && mkdir "$releases/latest" && cp -R "$dir" "$releases/latest/download"
}

test_installs_a_linux_release_from_before_the_icon() {
    fake_platform Linux x86_64
    publish_before_the_icon linux

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ -f $home/.local/share/applications/akshually.desktop ]] || fail "no launcher entry"
}

test_installs_a_macos_release_holding_the_old_named_bundle_as_akshually_app() {
    fake_platform Darwin arm64
    publish_before_the_icon macos

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_macos_install 1.2.3
    [[ $(ls -A "$home/Applications") == Akshually.app ]] || fail "left: $(ls -A "$home/Applications")"
}

test_installing_on_macos_leaves_no_old_named_bundle() {
    fake_platform Darwin arm64
    publish v1.2.3 latest
    mkdir -p "$home/Applications/akshually.app/Contents/MacOS"
    fake_binary "$home/Applications/akshually.app/Contents/MacOS/akshually" 0.0.7

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    assert_macos_install 1.2.3
    [[ $(ls -A "$home/Applications") == Akshually.app ]] || fail "left: $(ls -A "$home/Applications")"
}

# Packages with a stand-in binary for `$1` (`linux` or `macos`) and leaves the asset's path in `asset`.
package() {
    local build="$work/build/package"
    mkdir -p "$build"
    fake_binary "$build/akshually" 1.2.3
    asset=$("$root/packaging/package.sh" "$1" "$build/akshually" akshually-v1.2.3-test "$work/package" 2>/dev/null)
}

test_package_linux_holds_the_icons_and_a_launcher_entry_naming_them() {
    package linux

    mkdir "$work/unpacked"
    tar -C "$work/unpacked" -xzf "$asset"
    local dir="$work/unpacked/akshually-v1.2.3-test" icon size
    grep -qx 'Icon=akshually' "$dir/akshually.desktop" || fail "the launcher entry names no icon"
    grep -qx 'Name=Akshually' "$dir/akshually.desktop" || fail "the launcher entry isn't named Akshually"
    for icon in "$root"/packaging/icons/hicolor/*/apps/akshually.png; do
        size=$(basename "$(dirname "$(dirname "$icon")")")
        cmp -s "$icon" "$dir/icons/hicolor/$size/apps/akshually.png" || fail "the tarball lacks the $size icon"
    done
}

test_package_macos_holds_akshually_app_with_the_icon_its_plist_names() {
    package macos

    mkdir "$work/unpacked"
    unzip -q "$asset" -d "$work/unpacked"
    local contents="$work/unpacked/Akshually.app/Contents" icon
    [[ $(ls -A "$work/unpacked") == Akshually.app ]] || fail "the zip holds: $(ls -A "$work/unpacked")"
    [[ -x $contents/MacOS/akshually ]] || fail "no binary in the bundle"
    icon=$(awk '/<key>CFBundleIconFile<\/key>/ { getline; gsub(/<\/?string>|[ \t]/, ""); print }' "$contents/Info.plist")
    [[ -n $icon ]] || fail "Info.plist names no icon file"
    [[ -f $contents/Resources/${icon%.icns}.icns ]] || fail "Info.plist names $icon, which isn't in Resources"
    cmp -s "$root/packaging/icons/akshually.icns" "$contents/Resources/${icon%.icns}.icns" || fail "not the .icns"
    [[ $(grep -c '<string>Akshually</string>' "$contents/Info.plist") -eq 2 ]] || fail "the bundle names aren't Akshually"
}

test_installs_a_real_build() {
    if [[ -z ${AKSHUALLY_BINARY:-} ]]; then
        echo "skip (set AKSHUALLY_BINARY to a built akshually)"
        return
    fi
    local expected
    expected=$("$AKSHUALLY_BINARY" --version)
    publish "v${expected#akshually }" latest "$AKSHUALLY_BINARY"

    run_install

    [[ $status -eq 0 ]] || fail "install exited $status: $output"
    [[ $("$home/.local/bin/akshually" --version) == "$expected" ]] || fail "installed binary doesn't run"
}

# Plain arrays and `read` only: macOS ships bash 3.2, which has no `mapfile`.
if [[ $# -gt 0 ]]; then
    tests=("$@")
else
    tests=()
    while read -r name; do tests+=("$name"); done < <(declare -F | awk '$3 ~ /^test_/ { sub(/^test_/, "", $3); print $3 }')
fi
for name in "${tests[@]}"; do
    rm -rf "$work/fakebin" "$work/build" "$work/package" "$work/unpacked" "$work/old-release"
    home="$work/home-$name" releases="$work/releases-$name"
    mkdir -p "$home" "$releases"
    if ("test_$name"); then
        echo "ok   $name"
    else
        echo "FAIL $name"
        failures=$((failures + 1))
    fi
done
[[ $failures -eq 0 ]] || { echo "$failures failed" >&2; exit 1; }
