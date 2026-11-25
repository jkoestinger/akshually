<p align="center"><img src="packaging/icons/hicolor/256x256/apps/akshually.png" alt="Akshually's icon" width="128" height="128"></p>

# Akshually

A desktop code review app that keeps a human in the loop.

An agent can launch it on a diff and wait while you add comments, one round at a time, until you approve. You can also use it on its own to review pull/merge requests assigned to you on GitHub and GitLab.

> Status: early development.

## Install

On Linux (x86_64) or macOS (Apple Silicon or Intel):

```sh
curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash
```

It installs the latest stable release from [GitHub Releases](https://github.com/jkoestinger/akshually/releases), after checking it against the release's `SHA256SUMS`, and never asks for sudo. To install a given release, pre-releases included, name its tag: `curl -fsSL … | AKSHUALLY_VERSION=v0.1.0-rc.1 bash`. Run it again to replace the install in place.

There's one binary: it is both the `akshually` CLI that agents call and the Akshually app. Run it with no arguments to open the app. The script puts it here:

- **Linux**: `~/.local/bin/akshually`, and under your data home (`$XDG_DATA_HOME`, `~/.local/share` by default) the `applications/akshually.desktop` launcher entry and the icon in `icons/hicolor/`. The app rewrites the entry and the icon at launch when they are missing or out of date, so an install made before the icon gets it after its next update.
- **macOS**: `~/Applications/Akshually.app` (an `akshually.app` left by an older install is removed), and a `~/.local/bin/akshually` symlink to the binary inside it, so the CLI and the app stay one binary.

`~/.local/bin` must be on your `PATH`, both for agents and for the Linux launcher entry, which runs `akshually`. The script prints the line to add when it isn't; it doesn't edit your shell's startup files.

Check the install with `akshually --version`.

### Installing by hand

Download the asset for your platform and `SHA256SUMS` from [GitHub Releases](https://github.com/jkoestinger/akshually/releases), and check the asset with `sha256sum -c --ignore-missing SHA256SUMS` (`shasum -a 256 -c --ignore-missing SHA256SUMS` on macOS). Install it to the same places the script does, so that self-update can replace it later.

#### Linux (Wayland or X11)

The `x86_64-unknown-linux-gnu` tarball holds the `akshually` binary, an `akshually.desktop` launcher entry and the `icons/hicolor` tree with the icon. The binary is built on Ubuntu 24.04, so it needs glibc 2.39 or newer. It loads Wayland or X11, Vulkan and fontconfig at runtime, so a desktop system already has what it needs.

```sh
tar -xzf akshually-<version>-x86_64-unknown-linux-gnu.tar.gz
cd akshually-<version>-x86_64-unknown-linux-gnu
install -Dm755 akshually ~/.local/bin/akshually
data=${XDG_DATA_HOME:-$HOME/.local/share}
install -Dm644 akshually.desktop "$data/applications/akshually.desktop"
for icon in icons/hicolor/*/apps/akshually.png; do
    install -Dm644 "$icon" "$data/$icon"
done
```

#### macOS (Apple Silicon or Intel)

The `aarch64-apple-darwin` (Apple Silicon) and `x86_64-apple-darwin` (Intel) zips each hold `Akshually.app`. Unzip it and move it to `~/Applications`, then link the binary inside the bundle onto your `PATH`:

```sh
mkdir -p ~/Applications ~/.local/bin
rm -rf ~/Applications/akshually.app   # from an install made before the bundle was renamed
mv Akshually.app ~/Applications/
ln -sf ~/Applications/Akshually.app/Contents/MacOS/akshually ~/.local/bin/akshually
```

The app isn't signed with an Apple Developer ID or notarized, so Gatekeeper blocks a copy downloaded in a browser on first launch ("Akshually can't be opened" or "is damaged"). Clear the quarantine flag once:

```sh
xattr -dr com.apple.quarantine ~/Applications/Akshually.app
```

Alternatively, try to open it once, then choose **Open Anyway** in System Settings > Privacy & Security.

## Updating

Akshually looks for a newer release at launch and then once a day. When there is one, it shows a notice: **Update** installs it in place, after checking it against the release's `SHA256SUMS`, then **Restart** relaunches the app into it. Your Sessions are kept; an agent waiting in `akshually review` during the restart gets an error and must run it again, which the app warns about first. A stable install is offered stable releases only; a pre-release install is offered the newest release of either kind.

From a terminal, `akshually update` does the same install, and tells a running app, whose notice then offers **Restart**. Agents' commands never look for updates.

To turn the check off, add this to `config.toml` (`~/.config/akshually/` on Linux, `~/Library/Application Support/akshually/` on macOS):

```toml
check_for_updates = false
```

Self-update replaces the install the running `akshually` belongs to (a macOS bundle named `akshually.app` is renamed to `Akshually.app`), so it needs to be able to write there. An install it can't write to, such as an old copy in `/Applications`, is refused: reinstall with the install script instead.

## Teach your agent

`akshually agent-help` prints a guide for agents: the review loop, the commands and the exit codes. It is also an agent skill ([`skills/akshually/SKILL.md`](skills/akshually/SKILL.md)), so you can install it where your agent looks for skills, for example:

```sh
mkdir -p ~/.claude/skills/akshually
akshually agent-help > ~/.claude/skills/akshually/SKILL.md
```

Run it again after each update: the guide changes with the CLI, and an installed copy doesn't follow on its own.

Agents without skills can run `akshually agent-help` themselves: tell them to, in your project's agent instructions.

## License

Licensed under the [MIT license](LICENSE-MIT).
