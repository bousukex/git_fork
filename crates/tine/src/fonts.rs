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

fn monospace_font() -> Option<FcFont> {
    if let Some(f) = fc_match("monospace").filter(|f| f.monospace) {
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
    .find_map(|name| fc_match(name).filter(|f| f.family == name))
}

/// Builds font definitions with system fonts in front of egui's defaults.
pub fn definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();

    let ui = fc_match("sans-serif");
    let ui_ja = fc_match("sans-serif:lang=ja");
    let ui_bold = fc_match("sans-serif:weight=bold");
    let ui_bold_ja = fc_match("sans-serif:lang=ja:weight=bold");
    let mono = monospace_font();
    let mono_ja = fc_match("monospace:lang=ja");

    let add = |defs: &mut FontDefinitions, key: &str, font: &Option<FcFont>, weight: f32| {
        let Some(font) = font else { return None };
        let data = load(font, weight)?;
        log::info!("font {key}: {} ({})", font.family, font.file.display());
        defs.font_data.insert(key.to_owned(), Arc::new(data));
        Some(key.to_owned())
    };

    let regular: Vec<String> = [
        add(&mut defs, "sys-ui", &ui, 400.0),
        dedup(&ui_ja, &ui).then(|| add(&mut defs, "sys-ui-ja", &ui_ja, 400.0)).flatten(),
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
    fn rejects_missing_file() {
        assert!(parse_fc_match("/nonexistent.ttf\x1f0\x1fX\x1f").is_none());
    }
}
