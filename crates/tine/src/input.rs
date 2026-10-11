//! Keyboard input shared by every view: input-method (IME) composition
//! tracking and exact key matching.
//!
//! While an IME composes text (e.g. Japanese conversion), keys belong to
//! it: no shortcut, list key or dialog Enter may act on them. Composition
//! spans many frames, and not every frame carries an IME event, so the
//! state is tracked across frames: from a non-empty preedit until the
//! commit (or an empty preedit). The frame that ends a composition is
//! blocked too, since the Enter that confirmed it may arrive in it.

use egui::{Event, ImeEvent, Key, Modifiers};

const TRACK: &str = "tine-ime-composing";
const BLOCKED: &str = "tine-ime-blocked";
const LAYER: &str = "tine-input-layer";
const OWNER: &str = "tine-input-owner";

/// Who owns the keyboard, highest first. Only the owner handles keys; the
/// views (lists, search bar, commit box, history) only when nothing modal
/// is open. Decided at the start of a frame and again whenever something
/// may have opened a modal (results that arrived, a shortcut).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    /// A credential prompt (of any repository).
    Prompt,
    /// A tab's confirmation or operation dialog.
    Dialog,
    /// The settings dialog.
    Settings,
    QuickLaunch,
    /// Nothing modal: views and global shortcuts.
    #[default]
    Views,
}

pub fn set_layer(ctx: &egui::Context, layer: Layer) {
    set_owner(ctx, layer, 0);
}

/// Gives the keyboard to `layer`; `token` tells modals of one layer apart
/// (e.g. one tab dialog following another). A modal that takes the
/// keyboard gets none of the key presses of that frame: they were meant
/// for what was on screen before (Enter for the settings must not confirm
/// a dialog that just came up, Space with the Ctrl+, that opened the
/// settings must not stage the selected file).
pub fn set_owner(ctx: &egui::Context, layer: Layer, token: u64) {
    let before: Option<(Layer, u64)> = ctx.data(|d| d.get_temp(egui::Id::new(OWNER)));
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(LAYER), layer);
        d.insert_temp(egui::Id::new(OWNER), (layer, token));
    });
    if layer != Layer::Views && before != Some((layer, token)) {
        ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Key { pressed: true, .. })));
    }
}

pub fn layer(ctx: &egui::Context) -> Layer {
    ctx.data(|d| d.get_temp(egui::Id::new(LAYER))).unwrap_or_default()
}

/// A view (list, search bar, commit box, history) may act on keys: nothing
/// modal is open and no IME is composing.
pub fn views_have_keys(ctx: &egui::Context) -> bool {
    layer(ctx) == Layer::Views && !keys_blocked(ctx)
}

/// Updates the composition state from this frame's events. Call once per
/// frame before any key handling.
pub fn begin_frame(ctx: &egui::Context) {
    let mut composing: bool = ctx.data(|d| d.get_temp(egui::Id::new(TRACK))).unwrap_or(false);
    let mut blocked = composing;
    ctx.input(|i| {
        for e in &i.events {
            if let Event::Ime(ime) = e {
                match ime {
                    ImeEvent::Preedit { text, .. } if !text.is_empty() => {
                        composing = true;
                        blocked = true;
                    }
                    ImeEvent::Preedit { .. } | ImeEvent::Commit(_) => {
                        // Ending (or directly committing) blocks this frame.
                        blocked |= composing || matches!(ime, ImeEvent::Commit(_));
                        composing = false;
                    }
                    _ => {}
                }
            }
        }
    });
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(TRACK), composing);
        d.insert_temp(egui::Id::new(BLOCKED), blocked);
    });
    if blocked {
        // Keys that confirm, cancel or activate belong to the IME in this
        // frame; egui's own handling (a focused button's Enter/Space, a
        // modal's Esc) must not see them either.
        ctx.input_mut(|i| {
            i.events.retain(|e| !matches!(e, Event::Key { key: Key::Enter | Key::Escape | Key::Space | Key::Tab, .. }))
        });
    }
}

/// Keys must be left alone this frame (an IME is composing or just
/// finished).
pub fn keys_blocked(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(egui::Id::new(BLOCKED))).unwrap_or(false)
}

/// Modifier equality as Tine means it: exactly these, with Ctrl and the
/// platform's command key treated as one.
pub fn same_modifiers(a: Modifiers, b: Modifiers) -> bool {
    a.alt == b.alt && a.shift == b.shift && (a.ctrl || a.command || a.mac_cmd) == (b.ctrl || b.command || b.mac_cmd)
}

/// Whether `key` was pressed with exactly `modifiers` this frame (without
/// consuming it). Unlike egui's own checks, extra Shift or Alt don't match,
/// so Ctrl+Shift+P is never taken for Ctrl+P.
pub fn pressed(ctx: &egui::Context, modifiers: Modifiers, key: Key) -> bool {
    !keys_blocked(ctx)
        && ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, Event::Key { key: k, pressed: true, modifiers: m, .. } if *k == key && same_modifiers(*m, modifiers))
            })
        })
}

/// Like [`pressed`], and removes the key press so nothing else acts on it.
pub fn take_key(ctx: &egui::Context, modifiers: Modifiers, key: Key) -> bool {
    if keys_blocked(ctx) {
        return false;
    }
    ctx.input_mut(|i| {
        let at = i.events.iter().position(|e| {
            matches!(e, Event::Key { key: k, pressed: true, modifiers: m, .. } if *k == key && same_modifiers(*m, modifiers))
        });
        match at {
            Some(at) => {
                i.events.remove(at);
                true
            }
            None => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, events: Vec<Event>) -> bool {
        let input = egui::RawInput { events, ..Default::default() };
        let mut blocked = false;
        let mut out = ctx.run_ui(input, |ui| {
            begin_frame(ui.ctx());
            blocked = keys_blocked(ui.ctx());
        });
        out.textures_delta.clear();
        blocked
    }

    fn key(k: Key, modifiers: Modifiers) -> Event {
        Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    fn preedit(text: &str) -> Event {
        Event::Ime(ImeEvent::Preedit { text: text.into(), active_range_chars: None })
    }

    #[test]
    fn composition_blocks_across_frames() {
        let ctx = egui::Context::default();
        assert!(!frame(&ctx, vec![key(Key::P, Modifiers::COMMAND)]));
        assert!(frame(&ctx, vec![preedit("に")]));
        // Frames without IME events while composing stay blocked.
        assert!(frame(&ctx, vec![key(Key::Enter, Modifiers::NONE)]));
        assert!(frame(&ctx, vec![preedit("にほん")]));
        assert!(frame(&ctx, vec![]));
        // The confirming frame is blocked, the next one is free.
        assert!(frame(&ctx, vec![Event::Ime(ImeEvent::Commit("日本".into())), key(Key::Enter, Modifiers::NONE)]));
        assert!(!frame(&ctx, vec![key(Key::Enter, Modifiers::NONE)]));
        // Cancelling the conversion (empty preedit) ends it too.
        assert!(frame(&ctx, vec![preedit("か")]));
        assert!(frame(&ctx, vec![preedit("")]));
        assert!(!frame(&ctx, vec![]));
    }

    #[test]
    fn a_modal_gets_no_keys_in_the_frame_it_takes_the_keyboard() {
        let ctx = egui::Context::default();
        let enter = || key(Key::Enter, Modifiers::NONE);
        let run = |owner: (Layer, u64)| {
            let mut seen = false;
            let input = egui::RawInput { events: vec![enter()], ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                begin_frame(ui.ctx());
                set_owner(ui.ctx(), owner.0, owner.1);
                seen = pressed(ui.ctx(), Modifiers::NONE, Key::Enter);
            });
            out.textures_delta.clear();
            seen
        };
        assert!(run((Layer::Views, 0)));
        assert!(!run((Layer::Dialog, 1)), "a dialog that just came up");
        assert!(run((Layer::Dialog, 1)), "the same dialog, a frame later");
        assert!(!run((Layer::Dialog, 2)), "another dialog in its place");
        assert!(!run((Layer::Prompt, 7)));
        assert!(!run((Layer::Settings, 0)), "the settings again, after the prompt");
        assert!(run((Layer::Views, 0)), "keys go back to the views at once");
    }

    #[test]
    fn modifiers_match_exactly() {
        let ctrl_shift = Modifiers { shift: true, ..Modifiers::COMMAND };
        assert!(same_modifiers(Modifiers::CTRL, Modifiers::COMMAND));
        assert!(!same_modifiers(ctrl_shift, Modifiers::COMMAND));
        assert!(!same_modifiers(Modifiers::SHIFT, Modifiers::NONE));
    }
}
