# akshually

A desktop code review app that keeps a human in the loop.

An agent can launch it on a diff and wait while you add comments, one round at a time, until you approve. You can also use it on its own to review pull/merge requests assigned to you on GitHub and GitLab.

> Status: early development.

## Install

On Linux (x86_64) or macOS (Apple Silicon or Intel):

```sh
curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash
```

It installs the latest stable release from [GitHub Releases](https://github.com/jkoestinger/akshually/releases), after checking it against the release's `SHA256SUMS`, and never asks for sudo. To install a given release, pre-releases included, name its tag: `curl -fsSL … | AKSHUALLY_VERSION=v0.1.0-rc.1 bash`. Run it again to replace the install in place.

There's one binary: it is both the `akshually` CLI that agents call and the app. Run it with no arguments to open the app. The script puts it here:

- **Linux**: `~/.local/bin/akshually`, and the `~/.local/share/applications/akshually.desktop` launcher entry.
- **macOS**: `~/Applications/akshually.app`, and a `~/.local/bin/akshually` symlink to the binary inside it, so the CLI and the app stay one binary.

`~/.local/bin` must be on your `PATH`, both for agents and for the Linux launcher entry, which runs `akshually`. The script prints the line to add when it isn't; it doesn't edit your shell's startup files.

Check the install with `akshually --version`.

### Installing by hand

Download the asset for your platform and `SHA256SUMS` from [GitHub Releases](https://github.com/jkoestinger/akshually/releases), and check the asset with `sha256sum -c --ignore-missing SHA256SUMS` (`shasum -a 256 -c --ignore-missing SHA256SUMS` on macOS). Install it to the same places the script does, so that self-update can replace it later.

#### Linux (Wayland or X11)

The `x86_64-unknown-linux-gnu` tarball holds the `akshually` binary and an `akshually.desktop` launcher entry. The binary is built on Ubuntu 24.04, so it needs glibc 2.39 or newer. It loads Wayland or X11, Vulkan and fontconfig at runtime, so a desktop system already has what it needs.

```sh
tar -xzf akshually-<version>-x86_64-unknown-linux-gnu.tar.gz
cd akshually-<version>-x86_64-unknown-linux-gnu
install -Dm755 akshually ~/.local/bin/akshually
install -Dm644 akshually.desktop ~/.local/share/applications/akshually.desktop
```

#### macOS (Apple Silicon or Intel)

The `aarch64-apple-darwin` (Apple Silicon) and `x86_64-apple-darwin` (Intel) zips each hold `akshually.app`. Unzip it and move it to `~/Applications`, then link the binary inside the bundle onto your `PATH`:

```sh
mkdir -p ~/Applications ~/.local/bin
mv akshually.app ~/Applications/
ln -sf ~/Applications/akshually.app/Contents/MacOS/akshually ~/.local/bin/akshually
```

The app isn't signed with an Apple Developer ID or notarized, so Gatekeeper blocks a copy downloaded in a browser on first launch ("akshually can't be opened" or "is damaged"). Clear the quarantine flag once:

```sh
xattr -dr com.apple.quarantine ~/Applications/akshually.app
```

Alternatively, try to open it once, then choose **Open Anyway** in System Settings > Privacy & Security.

## Teach your agent

`akshually agent-help` prints a guide for agents: the review loop, the commands and the exit codes. It is also an agent skill ([`skills/akshually/SKILL.md`](skills/akshually/SKILL.md)), so you can install it where your agent looks for skills, for example:

```sh
mkdir -p ~/.claude/skills/akshually
akshually agent-help > ~/.claude/skills/akshually/SKILL.md
```

Agents without skills can run `akshually agent-help` themselves: tell them to, in your project's agent instructions.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
