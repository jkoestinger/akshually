//! The monospace font the Window's mono slot asks for: the system's own, without its slanted
//! faces (#71).

use std::path::PathBuf;

/// The mono slot when akshually registers no monospace font of its own: the platform's
/// monospaced families as Rune finds them, then Inter for what they lack.
pub(crate) const SYSTEM_MONO: &str = "ui-monospace, Inter";

/// The families Rune puts after fontconfig's own when it expands `ui-monospace` on Linux
/// (`SYSTEM_MONO_FAMILIES` in Rune's `system_fonts.rs`), so a character the family lacks still
/// renders in a monospaced font before Inter. Rune loads whichever are installed.
const MONO_FALLBACKS: &str = "\"DejaVu Sans Mono\", \"Liberation Mono\", \"Noto Sans Mono\", \"Ubuntu Mono\"";

/// A face of the family fontconfig matches for `monospace`.
pub struct MonoFace {
    /// The font file the face is in.
    pub path: PathBuf,
    /// fontconfig's slant for the face is roman. fontconfig derives it from the italic angle,
    /// so an oblique face whose OS/2 table says Regular isn't upright.
    pub upright: bool,
}

/// What the Window registers for its mono slot.
#[derive(Debug, PartialEq, Eq)]
pub struct MonoFonts {
    /// The mono slot, a CSS family list.
    pub slot: String,
    /// The font files to register, in order. Empty when the slot is `ui-monospace, Inter`.
    pub files: Vec<PathBuf>,
}

impl MonoFonts {
    /// The fallback: `ui-monospace, Inter`, registering nothing.
    fn system() -> Self {
        MonoFonts { slot: SYSTEM_MONO.to_owned(), files: Vec::new() }
    }
}

/// The mono slot and files for `family`, the family fontconfig matches for `monospace` (`None`
/// when it matches none), whose faces are `faces`: the family by name over its upright files
/// alone, so Rune can't pick a slanted face that says it is Regular, then Rune's other
/// monospaced families and Inter. A file is registered whole, so one that also holds a slanted
/// face (a collection) is left out.
///
/// Falls back to `ui-monospace, Inter` and no files when there's no upright file to register,
/// or when the family's name has both kinds of quote: a CSS family list has no escapes to name it.
pub fn upright_mono(family: Option<&str>, faces: &[MonoFace]) -> MonoFonts {
    let mut files: Vec<PathBuf> = Vec::new();
    for face in faces {
        let all_upright = faces.iter().filter(|other| other.path == face.path).all(|other| other.upright);
        if all_upright && !files.contains(&face.path) {
            files.push(face.path.clone());
        }
    }
    match family.and_then(css_family_name) {
        Some(name) if !files.is_empty() => MonoFonts { slot: format!("{name}, {MONO_FALLBACKS}, Inter"), files },
        _ => MonoFonts::system(),
    }
}

/// `family` quoted for a CSS family list, so Rune reads it as a name whatever it holds (a
/// leading digit, a generic's name). `None` when it has both kinds of quote.
fn css_family_name(family: &str) -> Option<String> {
    let quote = ['"', '\''].into_iter().find(|quote| !family.contains(*quote))?;
    Some(format!("{quote}{family}{quote}"))
}

/// The fonts the Window registers (`AppSpec::fonts`) and its mono slot.
pub(crate) struct WindowFonts {
    pub(crate) fonts: &'static [&'static [u8]],
    pub(crate) mono: &'static str,
}

impl WindowFonts {
    /// No fonts of akshually's own, and the mono slot `ui-monospace, Inter`.
    const SYSTEM: WindowFonts = WindowFonts { fonts: &[], mono: SYSTEM_MONO };
}

/// On Linux, the upright faces of the family fontconfig matches for `monospace` and Inter,
/// with the mono slot naming that family; when there's none to register,
/// [`WindowFonts::SYSTEM`]. Blocks on fontconfig and on reading the font files: called once,
/// as the app starts.
#[cfg(target_os = "linux")]
pub(crate) fn window_fonts() -> WindowFonts {
    /// Inter as Rune bundles it: fonts in `AppSpec::fonts` replace Rune's own, and the sans
    /// slot must stay Inter.
    const INTER: [&[u8]; 3] = [
        include_bytes!("../assets/Inter-Regular.ttf"),
        include_bytes!("../assets/Inter-Medium.ttf"),
        include_bytes!("../assets/Inter-SemiBold.ttf"),
    ];
    let mono = fontconfig_mono();
    let mut files: Vec<Vec<u8>> = Vec::new();
    for path in &mono.files {
        match std::fs::read(path) {
            Ok(file) => files.push(file),
            // The system monospace font as Rune finds it still shows code, at worst slanted.
            Err(err) => {
                eprintln!("akshually: can't read the monospace font file {}: {err}", path.display());
                return WindowFonts::SYSTEM;
            }
        }
    }
    if files.is_empty() {
        return WindowFonts::SYSTEM;
    }
    // The Window holds its fonts for as long as the process runs.
    let fonts: Vec<&'static [u8]> =
        INTER.into_iter().chain(files.into_iter().map(|file| &*Box::leak(file.into_boxed_slice()))).collect();
    WindowFonts { fonts: Box::leak(fonts.into_boxed_slice()), mono: Box::leak(mono.slot.into_boxed_str()) }
}

/// [`WindowFonts::SYSTEM`]: the workaround is for fontconfig's monospace family, so macOS and
/// the rest keep Rune's own choice.
#[cfg(not(target_os = "linux"))]
pub(crate) fn window_fonts() -> WindowFonts {
    WindowFonts::SYSTEM
}

/// [`upright_mono`] for the family fontconfig matches for `monospace`, asked in process.
/// fontique gives a face fontconfig's slant when the file's own metadata says Regular. With no
/// `libfontconfig`, fontique's collection is empty and this is the fallback.
#[cfg(target_os = "linux")]
fn fontconfig_mono() -> MonoFonts {
    use fontique::{Collection, CollectionOptions, FontStyle, GenericFamily, SourceKind};

    let mut system = Collection::new(CollectionOptions { shared: false, system_fonts: true });
    let id = system.generic_families(GenericFamily::Monospace).next();
    let Some(family) = id.and_then(|id| system.family(id)) else {
        return MonoFonts::system();
    };
    let faces: Vec<MonoFace> = family
        .fonts()
        .iter()
        .filter_map(|font| match &font.source().kind {
            SourceKind::Path(path) => {
                Some(MonoFace { path: path.to_path_buf(), upright: font.style() == FontStyle::Normal })
            }
            SourceKind::Memory(_) => None,
        })
        .collect();
    upright_mono(Some(family.name()), &faces)
}
