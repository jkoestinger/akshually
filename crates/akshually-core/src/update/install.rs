//! Replacing the install the running executable belongs to with a release's asset: a Linux
//! binary by an atomic rename, a macOS app bundle as a whole.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::{Release, Releases, UpdateError, Version, release_offered};

/// The platform a release asset is built for: its target triple, and how it is packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    triple: String,
    archive: Archive,
}

/// How a release asset is packed, as `packaging/package.sh` makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archive {
    /// Linux: `akshually-<tag>-<target>/akshually` in a `.tar.gz`.
    TarGz,
    /// macOS: `Akshually.app` (`akshually.app` before the rename) in a `.zip`.
    Zip,
}

impl Target {
    pub fn new(triple: &str, archive: Archive) -> Self {
        Target { triple: triple.to_owned(), archive }
    }

    /// The platform this binary runs on, among those the release workflow builds; `None` on
    /// any other.
    pub fn current() -> Option<Target> {
        let (triple, archive) = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => ("x86_64-unknown-linux-gnu", Archive::TarGz),
            ("macos", "aarch64") => ("aarch64-apple-darwin", Archive::Zip),
            ("macos", "x86_64") => ("x86_64-apple-darwin", Archive::Zip),
            _ => return None,
        };
        Some(Target::new(triple, archive))
    }

    /// The name of `tag`'s asset for this platform, as the release workflow names it.
    fn asset(&self, tag: &str) -> String {
        let ext = match self.archive {
            Archive::TarGz => "tar.gz",
            Archive::Zip => "zip",
        };
        format!("akshually-{tag}-{}.{ext}", self.triple)
    }
}

/// The name a macOS bundle has after an update, whatever it was called before.
const BUNDLE: &str = "Akshually.app";
/// The name releases from before the rename give the bundle in their zip.
const OLD_BUNDLE: &str = "akshually.app";
/// The binary's path inside a bundle.
const BUNDLE_BINARY: &str = "Contents/MacOS/akshually";

/// What an update replaces: the binary itself, or the macOS bundle it is inside.
enum Install {
    Binary(PathBuf),
    Bundle(PathBuf),
}

impl Install {
    /// The install `exe` (a real path, symlinks resolved) belongs to: the `.app` bundle when it
    /// is a bundle's `Contents/MacOS` binary, else the binary. The bundle may have any name, but
    /// it is a directory with the `.app` extension.
    fn containing(exe: &Path) -> Install {
        let bundle = exe
            .parent()
            .filter(|d| d.ends_with("Contents/MacOS"))
            .and_then(|d| d.parent()?.parent())
            .filter(|app| app.extension().is_some_and(|ext| ext == "app"));
        match bundle {
            Some(app) => Install::Bundle(app.to_owned()),
            None => Install::Binary(exe.to_owned()),
        }
    }

    /// The directory the install is in, which the Update writes to.
    fn dir(&self) -> &Path {
        let (Install::Binary(path) | Install::Bundle(path)) = self;
        path.parent().unwrap_or(Path::new("/"))
    }

    /// Where the Update must be able to write, besides `dir`: a bundle moves out of `dir` as a
    /// whole, which needs write access to the bundle itself.
    fn also_written(&self) -> Option<&Path> {
        match self {
            Install::Binary(_) => None,
            Install::Bundle(app) => Some(app),
        }
    }
}

/// Where the running executable `exe` is after an Update has replaced its install: the same
/// path for a binary, and for a bundle the binary inside `Akshually.app`, which an Update names
/// so whatever the bundle was called before. The restart launches this.
pub fn installed_exe(exe: &Path) -> PathBuf {
    match Install::containing(exe) {
        Install::Binary(exe) => exe,
        Install::Bundle(app) => app.with_file_name(BUNDLE).join(BUNDLE_BINARY),
    }
}

/// Turns an error writing into the install at `path` into `NotWritable` when the user may not
/// write there, and into `Io` when something else went wrong (a full disk).
fn unwritable(path: &Path) -> impl Fn(io::Error) -> UpdateError + '_ {
    move |error| match error.kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            UpdateError::NotWritable { path: path.to_owned() }
        }
        _ => UpdateError::Io { path: path.to_owned(), source: error },
    }
}

/// Looks for a release newer than the running one, and installs it in place of the running
/// executable's install. Blocks on the network.
pub struct Updater {
    releases: Releases,
    /// `None` on a platform the release workflow doesn't build for.
    target: Option<Target>,
    /// The running executable's real path, symlinks resolved.
    exe: PathBuf,
    running: Version,
    /// The `akshually` symlink on `PATH` that points into a bundle, which an Update that renames
    /// the bundle repoints. `None` without a home directory.
    path_link: Option<PathBuf>,
}

impl Updater {
    /// Updates the install `exe` belongs to (a real path, symlinks resolved: on Linux, resolve
    /// it at startup, as `/proc/self/exe` names a replaced binary "(deleted)"), from
    /// `releases`, for this platform, as the version this binary was built as.
    pub fn new(releases: Releases, exe: PathBuf) -> Self {
        // The install script's contract (ADR 0003): it links `~/.local/bin/akshually` into the bundle.
        let path_link = dirs::home_dir().map(|home| home.join(".local/bin/akshually"));
        Updater { releases, target: Target::current(), exe, running: Version::running(), path_link }
    }

    /// Installs `target`'s assets instead of this platform's.
    pub fn target(mut self, target: Target) -> Self {
        self.target = Some(target);
        self
    }

    /// Repoints `link`, not `~/.local/bin/akshually`, when an Update renames the bundle it links to.
    pub fn path_link(mut self, link: PathBuf) -> Self {
        self.path_link = Some(link);
        self
    }

    /// The version the install runs now.
    pub fn running(&self) -> &Version {
        &self.running
    }

    /// The release this install is offered (see [`offered`]); `None` when it is current.
    pub fn newer(&self) -> Result<Option<Release>, UpdateError> {
        let releases = self.releases.list()?;
        Ok(release_offered(&self.running, &releases).cloned())
    }

    /// Replaces the install with `release`'s asset for this platform, once its hash matches
    /// `SHA256SUMS`. On any failure the install is left as it was. Refuses an install in a
    /// directory the user can't write to, pointing to the install script.
    pub fn install(&self, release: &Release) -> Result<(), UpdateError> {
        let install = Install::containing(&self.exe);
        let dir = install.dir();
        // Unpacked beside the install, so it moves in by a rename on the same file system.
        let staging = tempfile::Builder::new().prefix(".akshually-update").tempdir_in(dir).map_err(unwritable(dir))?;
        if let Some(path) = install.also_written() {
            tempfile::tempfile_in(path).map_err(unwritable(path))?;
        }
        let target = self.target.as_ref().ok_or(UpdateError::NoPlatform)?;
        let name = target.asset(&release.tag);
        let url = |name: &str| release.assets.iter().find(|a| a.name == name).map(|a| a.url.clone());
        let asset_url =
            url(&name).ok_or_else(|| UpdateError::NoAsset { tag: release.tag.clone(), name: name.clone() })?;
        let sums_url = url("SHA256SUMS").ok_or_else(|| UpdateError::NoChecksums(release.tag.clone()))?;

        let sums = String::from_utf8_lossy(&self.releases.download(&sums_url)?).into_owned();
        let expected = listed_hash(&sums, &name).ok_or_else(|| UpdateError::NoChecksum(name.clone()))?;
        let archive = self.releases.download(&asset_url)?;
        if !hex(&Sha256::digest(&archive)).eq_ignore_ascii_case(expected) {
            return Err(UpdateError::HashMismatch(name));
        }

        let packed = staging.path().join(&name);
        fs::write(&packed, &archive).map_err(io_at(&packed))?;
        let unpacked = staging.path().join("unpacked");
        fs::create_dir(&unpacked).map_err(io_at(&unpacked))?;
        unpack(target.archive, &packed, &unpacked)
            .map_err(|message| UpdateError::Unpack { name: name.clone(), message })?;
        match &install {
            Install::Binary(exe) => {
                let stem = name.strip_suffix(".tar.gz").unwrap_or(&name).to_owned();
                let binary = unpacked.join(&stem).join("akshually");
                if !binary.is_file() {
                    return Err(UpdateError::Unpack { name, message: format!("it holds no {stem}/akshually") });
                }
                fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).map_err(io_at(&binary))?;
                fs::rename(&binary, exe).map_err(io_at(exe))
            }
            Install::Bundle(app) => {
                let Some(new) = [BUNDLE, OLD_BUNDLE]
                    .into_iter()
                    .map(|bundle| unpacked.join(bundle))
                    .find(|new| new.join(BUNDLE_BINARY).is_file())
                else {
                    return Err(UpdateError::Unpack { name, message: format!("it holds no {BUNDLE}") });
                };
                let renamed = app.with_file_name(BUNDLE);
                swap_bundle(app, &renamed, &new, &staging.path().join("old.app"))?;
                if &renamed != app {
                    self.repoint_path_link(app, &renamed);
                }
                Ok(())
            }
        }
    }
}

/// Puts the bundle `new` at `to`, moving the old one at `app` to `old` (inside the work dir,
/// which goes with it). Two renames: if the second fails, the old bundle goes back, so `app` is
/// never half-written.
fn swap_bundle(app: &Path, to: &Path, new: &Path, old: &Path) -> Result<(), UpdateError> {
    fs::rename(app, old).map_err(io_at(app))?;
    if let Err(error) = fs::rename(new, to) {
        // Best-effort: if even this fails, the error below is all there is to say.
        let _ = fs::rename(old, app);
        return Err(io_at(app)(error));
    }
    Ok(())
}

impl Updater {
    /// After the bundle `from` became `to`, points the `PATH` symlink at the binary in `to`, when
    /// it pointed into `from`: on a case-sensitive volume the old path no longer exists.
    /// Best-effort: the Update is done, and re-running the install script fixes the link.
    fn repoint_path_link(&self, from: &Path, to: &Path) {
        let Some(link) = &self.path_link else { return };
        let Ok(target) = fs::read_link(link) else { return };
        if !target.starts_with(from) {
            return;
        }
        let new_target = to.join(BUNDLE_BINARY);
        let tmp = link.with_file_name(".akshually.new");
        // Clears a leftover from an earlier failed attempt, which would make `symlink` fail; if
        // there is none, or it can't be removed, `symlink` below reports what matters.
        let _ = fs::remove_file(&tmp);
        let result = std::os::unix::fs::symlink(&new_target, &tmp).and_then(|()| fs::rename(&tmp, link));
        if let Err(error) = result {
            eprintln!("akshually: can't repoint {} at {}: {error}", link.display(), new_target.display());
        }
    }
}

/// Unpacks `packed` into `into`, with the platform's own tools, as the install script does.
fn unpack(archive: Archive, packed: &Path, into: &Path) -> Result<(), String> {
    let mut cmd = match archive {
        Archive::TarGz => {
            let mut cmd = Command::new("tar");
            cmd.arg("-xzf").arg(packed).arg("-C").arg(into);
            cmd
        }
        // `ditto` keeps what a bundle needs (extended attributes, the signature's resources);
        // other systems only meet a bundle in tests.
        Archive::Zip if cfg!(target_os = "macos") => {
            let mut cmd = Command::new("ditto");
            cmd.arg("-x").arg("-k").arg(packed).arg(into);
            cmd
        }
        Archive::Zip => {
            let mut cmd = Command::new("unzip");
            cmd.arg("-q").arg(packed).arg("-d").arg(into);
            cmd
        }
    };
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| format!("can't run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

/// The hash `SHA256SUMS` lists for `name`: lines of `<hash>  <name>`, the name marked `*` in
/// binary mode.
fn listed_hash<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, listed) = line.split_once(char::is_whitespace)?;
        (listed.trim_start().trim_start_matches('*') == name).then_some(hash)
    })
}

/// `bytes` in lower-case hex, as `SHA256SUMS` lists hashes.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Turns an error writing the Update at `path` into an [`UpdateError`].
fn io_at(path: &Path) -> impl Fn(io::Error) -> UpdateError + '_ {
    move |source| UpdateError::Io { path: path.to_owned(), source }
}
