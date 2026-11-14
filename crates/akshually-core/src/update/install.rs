//! Replacing the install the running executable belongs to with a release's asset: a Linux
//! binary by an atomic rename, a macOS `akshually.app` bundle as a whole.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::{Release, Releases, UpdateError, Version, offered};

/// The install script, which installs where self-update can write (ADR 0003).
const INSTALL_SCRIPT: &str =
    "curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash";

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
    /// macOS: `akshually.app` in a `.zip`.
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

/// What an update replaces: the binary itself, or the macOS bundle it is inside.
enum Install {
    Binary(PathBuf),
    Bundle(PathBuf),
}

impl Install {
    /// The install `exe` (a real path, symlinks resolved) belongs to: the `.app` bundle when it
    /// is a bundle's `Contents/MacOS` binary, else the binary.
    fn of(exe: &Path) -> Install {
        let bundle = exe
            .parent()
            .filter(|d| d.ends_with("Contents/MacOS"))
            .and_then(|d| d.parent()?.parent())
            .filter(|app| app.extension().is_some_and(|e| e == "app"));
        match bundle {
            Some(app) => Install::Bundle(app.to_owned()),
            None => Install::Binary(exe.to_owned()),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Install::Binary(path) | Install::Bundle(path) => path,
        }
    }

    /// The directory the install is in, which the update writes to.
    fn dir(&self) -> &Path {
        self.path().parent().unwrap_or(Path::new("/"))
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
}

impl Updater {
    /// Updates the install `exe` belongs to (a real path, symlinks resolved: on Linux, resolve
    /// it at startup, as `/proc/self/exe` names a replaced binary "(deleted)"), from
    /// `releases`, for this platform, as the version this binary was built as.
    pub fn new(releases: Releases, exe: PathBuf) -> Self {
        Updater { releases, target: Target::current(), exe, running: Version::running() }
    }

    /// Installs `target`'s assets instead of this platform's.
    pub fn target(mut self, target: Target) -> Self {
        self.target = Some(target);
        self
    }

    /// The version the install runs now.
    pub fn running(&self) -> &Version {
        &self.running
    }

    /// The release this install is offered (see [`offered`]); `None` when it is current.
    pub fn newer(&self) -> Result<Option<Release>, UpdateError> {
        let releases = self.releases.list()?;
        Ok(offered(&self.running, &releases).cloned())
    }

    /// Replaces the install with `release`'s asset for this platform, once its hash matches
    /// `SHA256SUMS`. On any failure the install is left as it was. Refuses an install in a
    /// directory the user can't write to, pointing to the install script.
    pub fn install(&self, release: &Release) -> Result<(), UpdateError> {
        let install = Install::of(&self.exe);
        let dir = install.dir();
        // Unpacked beside the install, so it moves in by a rename on the same file system.
        let work = tempfile::Builder::new()
            .prefix(".akshually-update")
            .tempdir_in(dir)
            .map_err(|_| UpdateError::NotWritable { path: dir.to_owned(), script: INSTALL_SCRIPT })?;
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

        let packed = work.path().join(&name);
        fs::write(&packed, &archive).map_err(io_at(&packed))?;
        let unpacked = work.path().join("unpacked");
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
                let new = unpacked.join("akshually.app");
                if !new.join("Contents/MacOS/akshually").is_file() {
                    return Err(UpdateError::Unpack { name, message: "it holds no akshually.app".into() });
                }
                swap_bundle(app, &new, &work.path().join("old.app"))
            }
        }
    }
}

/// Puts the bundle `new` at `app`, moving the old one to `old` (inside the work dir, which goes
/// with it). Two renames: if the second fails, the old bundle goes back, so `app` is never
/// half-written.
fn swap_bundle(app: &Path, new: &Path, old: &Path) -> Result<(), UpdateError> {
    fs::rename(app, old).map_err(io_at(app))?;
    if let Err(error) = fs::rename(new, app) {
        // Best-effort: if even this fails, the error below is all there is to say.
        let _ = fs::rename(old, app);
        return Err(io_at(app)(error));
    }
    Ok(())
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> UpdateError + '_ {
    move |source| UpdateError::Io { path: path.to_owned(), source }
}
