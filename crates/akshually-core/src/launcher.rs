//! Keeping the Linux launcher entry and its icon current for installs that self-update can't
//! reach: an update replaces only the binary, so the binary carries the files itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The launcher entry this version ships, as `packaging/linux/akshually.desktop`.
const DESKTOP_ENTRY: &[u8] = include_bytes!("../../../packaging/linux/akshually.desktop");

/// The icon at each `hicolor` size this version ships, as `packaging/icons/hicolor`.
const ICONS: [(&str, &[u8]); 7] = [
    ("16x16", include_bytes!("../../../packaging/icons/hicolor/16x16/apps/akshually.png")),
    ("32x32", include_bytes!("../../../packaging/icons/hicolor/32x32/apps/akshually.png")),
    ("48x48", include_bytes!("../../../packaging/icons/hicolor/48x48/apps/akshually.png")),
    ("64x64", include_bytes!("../../../packaging/icons/hicolor/64x64/apps/akshually.png")),
    ("128x128", include_bytes!("../../../packaging/icons/hicolor/128x128/apps/akshually.png")),
    ("256x256", include_bytes!("../../../packaging/icons/hicolor/256x256/apps/akshually.png")),
    ("512x512", include_bytes!("../../../packaging/icons/hicolor/512x512/apps/akshually.png")),
];

/// Brings the launcher entry and the icon under `data_home` (the user's XDG data home) up to
/// this version's, when an `akshually` launcher entry is installed there already: a missing or
/// different file is replaced. Writes nothing for a binary with no launcher entry (a hand-run or
/// development build). Returns the files it wrote.
///
/// Fails at the first file it can't write; the caller logs it and goes on, since the launcher is
/// cosmetic and must never hold up the app.
pub fn repair_launcher(data_home: &Path) -> io::Result<Vec<PathBuf>> {
    let entry = data_home.join("applications/akshually.desktop");
    if !entry.is_file() {
        return Ok(Vec::new());
    }
    let mut files = vec![(entry, DESKTOP_ENTRY)];
    for (size, bytes) in ICONS {
        files.push((data_home.join(format!("icons/hicolor/{size}/apps/akshually.png")), bytes));
    }
    let mut written = Vec::new();
    for (path, bytes) in files {
        if fs::read(&path).is_ok_and(|current| current == bytes) {
            continue;
        }
        write_replacing(&path, bytes)?;
        written.push(path);
    }
    Ok(written)
}

/// Writes `bytes` to `path` beside it and renames over it, so a reader never sees half a file.
fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.new"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        // Best-effort: the rename's error is the one to report.
        let _ = fs::remove_file(&tmp);
    })
}
