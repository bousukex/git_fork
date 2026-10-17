//! System font discovery via fontconfig (`fc-match`).
//!
//! egui ships only Latin fonts, so Japanese file names and commit messages
//! would render as tofu. We ask fontconfig for the user's configured
//! sans-serif / monospace fonts (plus their Japanese fallbacks) and put them
//! in front of egui's bundled fonts, which stay as the last-resort fallback
//! for emoji and symbols.

use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

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

/// Each question to fontconfig, and all of them at startup, within a time
/// (it may hang on a broken cache or font): past it, egui's own fonts.
const FC_MATCH_TIME: Duration = Duration::from_secs(2);
const FC_MATCH_TOTAL: Duration = Duration::from_secs(6);

/// Why the system's fonts weren't (all) used, if they weren't.
static TROUBLE: Mutex<Option<String>> = Mutex::new(None);

pub fn trouble() -> Option<String> {
    TROUBLE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// (Tests that look at, or change, the reason: one at a time.)
#[cfg(test)]
pub(crate) static TROUBLE_TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn forget_trouble() {
    *TROUBLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Notes why (the first reason is kept).
pub(crate) fn note_trouble(why: String) {
    log::warn!("fonts: {why}");
    TROUBLE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(why);
}

/// Asks fontconfig, with `program` (fc-match), each within `each` and all
/// within `total` from the first: once one doesn't answer (or the time is
/// up), no more are asked.
fn fc_matcher(program: std::ffi::OsString, each: Duration, total: Duration) -> impl Fn(&str) -> Option<FcFont> {
    let start = Instant::now();
    let gave_up = Mutex::new(false);
    move |pattern| {
        let mut gave_up = gave_up.lock().unwrap_or_else(|e| e.into_inner());
        if *gave_up {
            return None;
        }
        let left = total.saturating_sub(start.elapsed()).min(each);
        if left.is_zero() {
            *gave_up = true;
            note_trouble(format!("fontconfig took longer than {total:?} in all; egui's own fonts are used for the rest"));
            return None;
        }
        let mut cmd = Command::new(&program);
        cmd.args(["-f", "%{file}\x1f%{index}\x1f%{family[0]}\x1f%{spacing}", pattern]);
        let out = match pengitto_core::bounded::run(cmd, left) {
            Ok(out) => out,
            Err(e) => {
                *gave_up = true;
                note_trouble(format!("{e}; egui's own fonts are used"));
                return None;
            }
        };
        if !out.status.success() || !out.complete {
            note_trouble(format!("fc-match {pattern} failed ({})", out.status));
            return None;
        }
        let found = parse_fc_match(&out.stdout_text());
        if found.is_none() {
            note_trouble(format!("fc-match {pattern} told no font file there is: {:?}", out.stdout_text()));
        }
        found
    }
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
    definitions_from(&fc_matcher("fc-match".into(), FC_MATCH_TIME, FC_MATCH_TOTAL))
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

    /// A script at `path` (written by a child: no file of this process is
    /// left open for writing when other tests' threads fork).
    fn script(path: &std::path::Path, body: &str) {
        let ok = Command::new("sh")
            .args(["-c", "printf '#!/bin/sh\\n%s\\n' \"$2\" > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .arg(body)
            .status()
            .unwrap();
        assert!(ok.success());
    }

    #[test]
    fn fontconfig_that_doesnt_answer_is_given_up_on() {
        let _one = TROUBLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let font = dir.path().join("a.ttf");
        std::fs::write(&font, "").unwrap();
        let answering = dir.path().join("answering");
        script(&answering, &format!("printf '%s\\037%s\\037%s\\037' '{}' 0 A", font.display()));
        let lookup = fc_matcher(answering.into(), Duration::from_secs(5), Duration::from_secs(10));
        assert_eq!(lookup("sans-serif").map(|f| f.family), Some("A".into()));
        // One hangs: given up on within its time, none asked after it.
        let hanging = dir.path().join("hanging");
        script(&hanging, "exec sleep 30");
        forget_trouble();
        let lookup = fc_matcher(hanging.into(), Duration::from_millis(300), Duration::from_secs(10));
        let t = Instant::now();
        assert!(lookup("sans-serif").is_none() && lookup("monospace").is_none() && lookup("x").is_none());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(trouble().is_some_and(|t| t.contains("didn't answer")), "{:?}", trouble());
        // All of them within the total: each slow one gets what is left.
        let slow = dir.path().join("slow");
        script(&slow, &format!("sleep 0.3; printf '%s\\037%s\\037%s\\037' '{}' 0 A", font.display()));
        forget_trouble();
        let lookup = fc_matcher(slow.into(), Duration::from_secs(5), Duration::from_millis(800));
        let t = Instant::now();
        let found = (0..10).filter(|_| lookup("p").is_some()).count();
        assert!((1..10).contains(&found), "{found}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(trouble().is_some_and(|t| t.contains("in all") || t.contains("didn't answer")), "{:?}", trouble());
        // No time left for it at all: noted.
        forget_trouble();
        let answering = dir.path().join("answering");
        assert!(fc_matcher(answering.into(), Duration::from_secs(5), Duration::ZERO)("sans-serif").is_none());
        assert!(trouble().is_some_and(|t| t.contains("in all")), "{:?}", trouble());
        // Failing, or telling of no file: noted too (the first reason).
        forget_trouble();
        let failing = dir.path().join("failing");
        script(&failing, "echo broken >&2; exit 1");
        assert!(fc_matcher(failing.into(), Duration::from_secs(5), Duration::from_secs(10))("sans-serif").is_none());
        assert!(trouble().is_some_and(|t| t.contains("fc-match sans-serif failed")), "{:?}", trouble());
        forget_trouble();
        let nowhere = dir.path().join("nowhere");
        script(&nowhere, "printf '%s\\037%s\\037%s\\037' /nonexistent.ttf 0 A");
        assert!(fc_matcher(nowhere.into(), Duration::from_secs(5), Duration::from_secs(10))("monospace").is_none());
        assert!(trouble().is_some_and(|t| t.contains("no font file")), "{:?}", trouble());
        forget_trouble();
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

