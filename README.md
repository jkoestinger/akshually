# akshually

A desktop code review app that keeps a human in the loop.

An agent can launch it on a diff and wait while you add comments, one round at a time, until you approve. You can also use it on its own to review pull/merge requests assigned to you on GitHub and GitLab.

> Status: early development.

## Install

Download the asset for your platform from [GitHub Releases](https://github.com/jkoestinger/akshually/releases). There's one binary: it is both the `akshually` CLI that agents call and the app. Run it with no arguments to open the app.

### Linux (Wayland or X11)

The `x86_64-unknown-linux-gnu` tarball holds the `akshually` binary and an `akshually.desktop` launcher entry. The binary is built on Ubuntu 24.04, so it needs glibc 2.39 or newer. It loads Wayland or X11, Vulkan and fontconfig at runtime, so a desktop system already has what it needs.

```sh
tar -xzf akshually-<version>-x86_64-unknown-linux-gnu.tar.gz
cd akshually-<version>-x86_64-unknown-linux-gnu
install -Dm755 akshually ~/.local/bin/akshually
install -Dm644 akshually.desktop ~/.local/share/applications/akshually.desktop
```

`~/.local/bin` must be on your `PATH`, both for agents and for the launcher entry, which runs `akshually`.

### macOS (Apple Silicon or Intel)

The `aarch64-apple-darwin` (Apple Silicon) and `x86_64-apple-darwin` (Intel) zips each hold `akshually.app`. Unzip it and move it to `/Applications`.

The app isn't signed with an Apple Developer ID or notarized, so Gatekeeper blocks it on first launch ("akshually can't be opened" or "is damaged"). Clear the quarantine flag once:

```sh
xattr -dr com.apple.quarantine /Applications/akshually.app
```

Alternatively, try to open it once, then choose **Open Anyway** in System Settings > Privacy & Security.

The binary inside the bundle is also the CLI. Put it on your `PATH` with a symlink rather than a copy, so the CLI and the app stay one binary:

```sh
sudo mkdir -p /usr/local/bin
sudo ln -sf /Applications/akshually.app/Contents/MacOS/akshually /usr/local/bin/akshually
```

Check either install with `akshually --version`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
