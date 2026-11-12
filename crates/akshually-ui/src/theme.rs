//! akshually's design tokens. `assets/theme.json` is their source of truth (the Figcraft
//! `akshually` variables mirror it, see design/README.md); this maps them onto the kit.

use std::sync::OnceLock;

use rune::theme::Theme;
use rune_kit::{Kit, PaletteSeed, StateColors};

const THEME_JSON: &str = include_str!("../assets/theme.json");

/// The tokens of one appearance. Colours are read through the theme, which resolves them for
/// the current blend mode.
pub(crate) fn tokens(dark: bool) -> &'static Theme {
    static DARK: OnceLock<Theme> = OnceLock::new();
    static LIGHT: OnceLock<Theme> = OnceLock::new();
    let cell = if dark { &DARK } else { &LIGHT };
    cell.get_or_init(|| Theme::from_json(THEME_JSON, dark).expect("assets/theme.json is a valid Rune theme"))
}

/// The kit for one appearance, built from [`tokens`]. Cached per appearance and blend mode, as
/// the kit resolves translucent tokens for the window's blend mode.
pub(crate) fn kit(dark: bool) -> &'static Kit {
    static KITS: [[OnceLock<Kit>; 2]; 2] = [[OnceLock::new(), OnceLock::new()], [OnceLock::new(), OnceLock::new()]];
    KITS[usize::from(dark)][usize::from(rune::theme::blends_srgb())].get_or_init(|| build_kit(tokens(dark)))
}

fn build_kit(t: &Theme) -> Kit {
    let (bg, fg, accent) = (t.color("bg"), t.color("fg"), t.color("accent"));
    let mut k = Kit::from_palette(PaletteSeed { bg, fg, accent });
    let c = &mut k.c;

    let (surface, raised, border, muted) =
        (t.color("surface"), t.color("surface-raised"), t.color("border"), t.color("fg-muted"));
    c.layer_01 = surface;
    c.surface_weak = surface;
    c.bg_button_neutral = surface;
    c.layer_02 = raised;
    c.surface_raised = raised;
    c.surface_raised_base = raised;
    c.border_base = border;
    c.border_weak = border;
    c.std_border = border;
    c.text_muted = muted;
    c.text_weak = muted;
    c.icon_muted = muted;
    c.std_icon = muted;

    // The diff view tints its added and removed lines from `icon` and counts them in `fg`.
    c.success = role(c.success, t.color("diff-add-bg"), t.color("diff-add-fg"));
    c.danger = role(c.danger, t.color("diff-del-bg"), t.color("diff-del-fg"));
    // The tab strip's attention marker.
    let attention = t.color("state-attention");
    c.warning = StateColors { fg: attention, icon: attention, ..c.warning };

    k.radius.sm = t.num("radius-sm");
    k.radius.md = t.num("radius-md");
    k
}

fn role(base: StateColors, bg: rune::Color, fg: rune::Color) -> StateColors {
    StateColors { bg, fg, icon: fg, ..base }
}
