# Akshually updates itself, and only the app checks for updates

Akshually installs into locations the user can always write to, with no sudo: `~/.local/bin` (plus the launcher entry) on Linux, and `~/Applications/Akshually.app` with a `~/.local/bin` symlink on macOS. It replaces its own install in place, from the app's update notice or from `akshually update`, after checking the release's `SHA256SUMS`. A one-line install script that only re-installs would barely beat downloading by hand, and an update that needs sudo couldn't run from the app. Only the app checks for a newer release, at launch and then daily, and only it shows a notice. The CLI calls agents make never touch the network for this and never print a hint: they block while the human reviews and their output is parsed by agents, so a check there would add latency and noise for a reader who can't act on it. The check can be turned off with `check_for_updates = false` in `config.toml`.

## Considered Options

- **A "new version" hint on CLI calls**: reaches people who never open the app, but agents read that output, not humans.
- **Only an update check, with "re-run the install script" as the remedy**: simpler, but leaves the human to do by hand what the app can do itself.
- **Installing into `/Applications` and `/usr/local/bin`, as the README first did**: conventional on macOS, but needs sudo or an admin account, so self-update could fail on an install it made itself.

## Consequences

- An update refuses an install it can't write to (an old `/Applications` copy, a future package-manager install) and points to the install script. A packaged Akshually (Homebrew, AUR) would need a way to turn self-update off.
- The CLI and the app speak a versioned protocol, so after an update the running app must restart before new CLI calls can reach it. Agents waiting in `review` during that restart lose their call and must run it again; Sessions survive on disk.
