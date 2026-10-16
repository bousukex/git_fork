//! System font discovery via fontconfig (`fc-match`).
//!
//! egui ships only Latin fonts, so Japanese file names and commit messages
//! would render as tofu. We ask fontconfig for the user's configured
//! sans-serif / monospace fonts (plus their Japanese fallbacks) and put them
//! in front of egui's bundled fonts, which stay as the last-resort fallback
//! for emoji and symbols.

use std::{path::PathBuf, process::Command, sync::Arc};

use egui::{
    FontData, FontDefinitions, FontFamily,
    epaint::text::{FontTweak, VariationCoords},
};

/// Family name used for bold UI text (`FontFamily::Name(BOLD)`).
pub const BOLD: &str = "bold";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FcFont {
    file: PathBuf,
    /// Face index inside a font collection (`.ttc`).
    face: u32,
    family: String,
    monospace: bool,
}

fn fc_match(pattern: &str) -> Option<FcFont> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}\x1f%{index}\x1f%{family[0]}\x1f%{spacing}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_fc_match(&String::from_utf8_lossy(&out.stdout))
}

fn parse_fc_match(s: &str) -> Option<FcFont> {
    let mut it = s.split('\x1f');
    let file = PathBuf::from(it.next()?);
    // fontconfig packs the named-instance number of variable fonts into the
    // upper 16 bits; the face index is the lower half.
    let face = it.next()?.trim().parse::<u32>().ok()? & 0xFFFF;
    let family = it.next()?.to_owned();
    let monospace = it.next().is_some_and(|sp| sp.trim() == "100");
    file.is_file().then_some(FcFont { file, face, family, monospace })
}

/// Fonts are loaded once per process and live for its whole lifetime, so we
/// memory-map them and leak the mapping instead of copying ~30MB CJK files.
fn load(font: &FcFont, weight: f32) -> Option<FontData> {
    let file = std::fs::File::open(&font.file).ok()?;
    // SAFETY: system font files are not modified while we run; a concurrent
    // package upgrade could at worst produce garbled glyphs.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    let mut data = FontData::from_static(bytes);
    data.index = font.face;
    if data.variation_axes().iter().any(|a| a.tag == *b"wght") {
        data.tweak = FontTweak {
            coords: VariationCoords::new([("wght", weight)]),
            ..Default::default()
        };
    }
    Some(data)
}

fn monospace_font(lookup: &dyn Fn(&str) -> Option<FcFont>) -> Option<FcFont> {
    if let Some(f) = lookup("monospace").filter(|f| f.monospace) {
        return Some(f);
    }
    // Some distro defaults alias `monospace` to a proportional CJK font.
    // Try well-known monospace families and keep the first real match.
    [
        "Noto Sans Mono",
        "Adwaita Mono",
        "JetBrains Mono",
        "Hack",
        "DejaVu Sans Mono",
        "Source Code Pro",
        "Liberation Mono",
    ]
    .into_iter()
    .find_map(|name| lookup(name).filter(|f| f.family == name))
}

/// Symbols the UI draws. The default font may lack some (arrows, check
/// marks…): a font that has them all is added as a fallback.
pub const UI_SYMBOLS: &str = "✓×⊘▶?…→←▼↑↓☰−■!";

/// A fontconfig pattern for a sans-serif font with every one of `chars`.
fn with_chars(chars: &str) -> String {
    let codes: Vec<String> = chars.chars().map(|c| format!("{:x}", c as u32)).collect();
    format!("sans-serif:charset={}", codes.join(" "))
}

/// Builds font definitions with system fonts in front of egui's defaults.
pub fn definitions() -> FontDefinitions {
    definitions_from(&fc_match)
}

/// The same, with the fonts `lookup` finds for fontconfig patterns.
fn definitions_from(lookup: &dyn Fn(&str) -> Option<FcFont>) -> FontDefinitions {
    let mut defs = FontDefinitions::default();

    let ui = lookup("sans-serif");
    let ui_ja = lookup("sans-serif:lang=ja");
    let ui_bold = lookup("sans-serif:weight=bold");
    let ui_bold_ja = lookup("sans-serif:lang=ja:weight=bold");
    let mono = monospace_font(lookup);
    let mono_ja = lookup("monospace:lang=ja");
    let symbols = lookup(&with_chars(UI_SYMBOLS));

    let add = |defs: &mut FontDefinitions, key: &str, font: &Option<FcFont>, weight: f32| {
        let Some(font) = font else { return None };
        let data = load(font, weight)?;
        log::info!("font {key}: {} ({})", font.family, font.file.display());
        defs.font_data.insert(key.to_owned(), Arc::new(data));
        Some(key.to_owned())
    };

    let symbols_new = dedup(&symbols, &ui) && dedup(&symbols, &ui_ja);
    let regular: Vec<String> = [
        add(&mut defs, "sys-ui", &ui, 400.0),
        dedup(&ui_ja, &ui).then(|| add(&mut defs, "sys-ui-ja", &ui_ja, 400.0)).flatten(),
        symbols_new.then(|| add(&mut defs, "sys-symbols", &symbols, 400.0)).flatten(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let bold: Vec<String> = [
        add(&mut defs, "sys-ui-bold", &ui_bold, 700.0),
        dedup(&ui_bold_ja, &ui_bold)
            .then(|| add(&mut defs, "sys-ui-bold-ja", &ui_bold_ja, 700.0))
            .flatten(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let monospace: Vec<String> = [
        add(&mut defs, "sys-mono", &mono, 400.0),
        dedup(&mono_ja, &mono).then(|| add(&mut defs, "sys-mono-ja", &mono_ja, 400.0)).flatten(),
    ]
    .into_iter()
    .flatten()
    .collect();

    let fallback_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let prepend = |defs: &mut FontDefinitions, family: FontFamily, fonts: &[String]| {
        let list = defs.families.entry(family).or_default();
        for (i, f) in fonts.iter().enumerate() {
            list.insert(i, f.clone());
        }
    };
    prepend(&mut defs, FontFamily::Proportional, &regular);
    // Japanese glyphs in code/diffs fall back to the UI CJK font when no
    // monospace CJK font exists.
    let mut mono_chain = monospace;
    mono_chain.extend(regular.iter().cloned());
    prepend(&mut defs, FontFamily::Monospace, &mono_chain);

    let mut bold_chain = if bold.is_empty() { regular.clone() } else { bold };
    bold_chain.extend(regular.iter().filter(|f| *f == "sys-symbols").cloned());
    bold_chain.extend(fallback_prop);
    defs.families.insert(FontFamily::Name(BOLD.into()), bold_chain);

    defs
}

/// True when `a` resolves to a different face than `b` (worth loading).
fn dedup(a: &Option<FcFont>, b: &Option<FcFont>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.file != b.file || a.face != b.face,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fc_match_output_and_strips_named_instance() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().display().to_string();
        let f = parse_fc_match(&format!("{path}\x1f262145\x1fNoto Sans CJK JP\x1f")).unwrap();
        assert_eq!(f.face, 1);
        assert_eq!(f.family, "Noto Sans CJK JP");
        assert!(!f.monospace);
        let m = parse_fc_match(&format!("{path}\x1f0\x1fHack\x1f100")).unwrap();
        assert!(m.monospace);
    }

    #[test]
    fn asks_for_a_font_with_the_symbols() {
        assert_eq!(with_chars("✓→!"), "sans-serif:charset=2713 2192 21");
    }

    #[test]
    fn rejects_missing_file() {
        assert!(parse_fc_match("/nonexistent.ttf\x1f0\x1fX\x1f").is_none());
    }

    /// The chains built from fixed fonts (egui's own, as files) whatever the
    /// machine has: Japanese and symbol fallbacks where they go, a
    /// `monospace` that is proportional passed over, nothing loaded twice.
    #[test]
    fn the_chains_from_fixed_fonts() {
        let dir = tempfile::tempdir().unwrap();
        let egui_fonts = FontDefinitions::default();
        let file = |name: &str, key: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, &*egui_fonts.font_data[key].font).unwrap();
            p
        };
        let (ui, ja, mono, symbols) = (
            file("ui.ttf", "Ubuntu-Light"),
            file("ja.ttf", "NotoEmoji-Regular"),
            file("mono.ttf", "Hack"),
            file("symbols.ttf", "emoji-icon-font"),
        );
        let font = |file: &PathBuf, family: &str, monospace: bool| Some(FcFont { file: file.clone(), face: 0, family: family.into(), monospace });
        let fixture = |symbols_font: Option<FcFont>| {
            let (ui, ja, mono) = (ui.clone(), ja.clone(), mono.clone());
            move |pattern: &str| match pattern {
                "sans-serif" | "sans-serif:weight=bold" => font(&ui, "Ubuntu", false),
                "sans-serif:lang=ja" | "sans-serif:lang=ja:weight=bold" | "monospace:lang=ja" => font(&ja, "Noto Sans CJK JP", false),
                // The default monospace is a proportional font: passed over.
                "monospace" => font(&ui, "Ubuntu", false),
                "Hack" => font(&mono, "Hack", true),
                p if p == with_chars(UI_SYMBOLS) => symbols_font.clone(),
                _ => None,
            }
        };
        let names = |defs: &FontDefinitions, family: FontFamily| -> Vec<String> {
            defs.families[&family].iter().filter(|k| k.starts_with("sys-")).cloned().collect()
        };
        let defs = definitions_from(&fixture(font(&symbols, "Symbols", false)));
        assert_eq!(names(&defs, FontFamily::Proportional), ["sys-ui", "sys-ui-ja", "sys-symbols"]);
        assert_eq!(names(&defs, FontFamily::Monospace), ["sys-mono", "sys-mono-ja", "sys-ui", "sys-ui-ja", "sys-symbols"]);
        assert_eq!(names(&defs, FontFamily::Name(BOLD.into())), ["sys-ui-bold", "sys-ui-bold-ja", "sys-symbols"]);
        assert_eq!(*defs.font_data["sys-mono"].font, *egui_fonts.font_data["Hack"].font);
        assert_eq!(*defs.font_data["sys-symbols"].font, *egui_fonts.font_data["emoji-icon-font"].font);
        // egui's fonts stay as the last fallbacks.
        assert!(defs.families[&FontFamily::Proportional].ends_with(&egui_fonts.families[&FontFamily::Proportional]));
        // The font with the symbols is the UI's own: not loaded again.
        let defs = definitions_from(&fixture(font(&ui, "Ubuntu", false)));
        assert!(!defs.font_data.contains_key("sys-symbols"));
        assert_eq!(names(&defs, FontFamily::Name(BOLD.into())), ["sys-ui-bold", "sys-ui-bold-ja"]);
        // No fonts at all: egui's, and bold falls back to them.
        let defs = definitions_from(&|_| None);
        assert_eq!(defs.families[&FontFamily::Proportional], egui_fonts.families[&FontFamily::Proportional]);
        assert_eq!(defs.families[&FontFamily::Name(BOLD.into())], egui_fonts.families[&FontFamily::Proportional]);
    }
}

#[cfg(test)]
mod glyph_tests {
    use egui::FontFamily;
    use skrifa::MetadataProvider as _;

    use super::UI_SYMBOLS;

    /// A symbol missing from every loaded font renders as an empty box, so
    /// check them against the real fonts, by their character maps (egui's
    /// own check compares faces, and calls a character missing when the
    /// first font that has it also has the replacement character).
    #[test]
    fn ui_symbols_have_glyphs() {
        let defs = super::definitions();
        let chain = &defs.families[&FontFamily::Proportional];
        let missing: String = UI_SYMBOLS
            .chars()
            .filter(|&c| {
                !chain.iter().any(|key| {
                    let data = &defs.font_data[key];
                    skrifa::FontRef::from_index(&data.font, data.index).is_ok_and(|f| f.charmap().map(c).is_some())
                })
            })
            .collect();
        assert!(missing.is_empty(), "UI symbols without a glyph: {missing:?}");
    }
}

