//! The credential prompt dialog (see [`crate::askpass`]). Shown above
//! everything else, whichever tab started the operation, even a closed one.

use egui::{Key, RichText};

use crate::{
    askpass::{self, Answer, Prompt, PromptKind},
    theme::Palette,
};

/// Input of the prompt being answered.
#[derive(Clone, Default)]
struct Input {
    prompt: u64,
    value: String,
    /// Show the typed value (secrets start hidden).
    reveal: bool,
}

fn input_id() -> egui::Id {
    egui::Id::new("askpass-input")
}

/// Each prompt gets its own text field, so no state (like undo history)
/// carries over from one prompt to the next.
fn field_id(prompt: u64) -> egui::Id {
    egui::Id::new(("askpass-field", prompt))
}

/// Drops everything kept about what was typed for `prompt`.
fn forget_input(ctx: &egui::Context, prompt: u64) {
    ctx.data_mut(|d| {
        d.remove::<Input>(input_id());
        d.remove::<egui::text_edit::TextEditState>(field_id(prompt));
    });
}

/// Whether anything typed into a prompt is still kept (for tests).
pub fn input_kept(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| {
        let input: Option<Input> = d.get_temp(input_id());
        input.is_some_and(|i| !i.value.is_empty() || d.get_persisted::<egui::text_edit::TextEditState>(field_id(i.prompt)).is_some())
    })
}

/// Shows the oldest credential prompt, if any; returns whether one is shown.
pub fn show(ctx: &egui::Context) -> bool {
    let prompt = askpass::pending();
    let mut input: Option<Input> = ctx.data_mut(|d| d.get_temp(input_id()));
    // The prompt typed into has ended (answered here, or cancelled with its
    // operation): forget the input.
    if let Some(old) = &input
        && prompt.as_ref().is_none_or(|p| p.id != old.prompt)
    {
        forget_input(ctx, old.prompt);
        input = None;
    }
    let Some(prompt) = prompt else { return false };
    let mut input =
        input.unwrap_or_else(|| Input { prompt: prompt.id, value: String::new(), reveal: prompt.kind == PromptKind::Text });
    let mut answer = None;
    egui::Modal::new(egui::Id::new("askpass-dialog")).show(ctx, |ui| {
        ui.set_width(480.0);
        answer = body(ui, &prompt, &mut input);
    });
    // Typed text must not linger in the field's undo history.
    if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, field_id(prompt.id)) {
        state.clear_undoer();
        state.store(ctx, field_id(prompt.id));
    }
    if let Some(a) = answer {
        askpass::answer(prompt.id, a);
        forget_input(ctx, prompt.id);
    } else {
        ctx.data_mut(|d| d.insert_temp(input_id(), input));
    }
    true
}

fn body(ui: &mut egui::Ui, prompt: &Prompt, input: &mut Input) -> Option<Answer> {
    let p = Palette::of(ui.ctx());
    let title = match prompt.kind {
        PromptKind::HostKey => "Unknown host",
        PromptKind::Confirm => "Confirm",
        PromptKind::Secret | PromptKind::Text => "Credentials needed",
    };
    ui.heading(title);
    ui.label(RichText::new(format!("{} — {}", prompt.op_label, prompt.workdir.display())).small().color(p.text_weak));
    ui.add_space(6.0);
    ui.add(egui::Label::new(prompt.text.trim()).wrap().selectable(true));
    ui.add_space(6.0);
    let enter = ui.input(|i| i.key_pressed(Key::Enter) && !i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
    let mut out = None;
    match prompt.kind {
        PromptKind::HostKey => {
            ui.label(
                RichText::new(
                    "Check the fingerprint against one the server's owner published. Connecting stores the key in \
                     ~/.ssh/known_hosts.",
                )
                .small()
                .color(p.text_weak),
            );
            ui.add(
                egui::TextEdit::singleline(&mut input.value)
                    .id(field_id(prompt.id))
                    .hint_text("Or paste the expected fingerprint (SHA256:…)")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fingerprint = input.value.trim().to_owned();
                let label = if fingerprint.is_empty() { "Yes, Connect" } else { "Check Fingerprint" };
                if ui.button(RichText::new(label).strong()).clicked() {
                    out = Some(Answer::Value(if fingerprint.is_empty() { "yes".into() } else { fingerprint }));
                }
                if ui.button("No").clicked() {
                    out = Some(Answer::Value("no".into()));
                }
                if ui.button("Cancel").clicked() {
                    out = Some(Answer::Cancel);
                }
            });
        }
        PromptKind::Confirm => {
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("Yes").strong()).clicked() || enter {
                    out = Some(Answer::Value(String::new()));
                }
                if ui.button("No").clicked() {
                    out = Some(Answer::Decline);
                }
            });
        }
        PromptKind::Secret | PromptKind::Text => {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut input.value)
                    .id(field_id(prompt.id))
                    .password(!input.reveal)
                    .desired_width(f32::INFINITY),
            );
            if !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
                resp.request_focus();
            }
            ui.checkbox(&mut input.reveal, "Show");
            ui.label(
                RichText::new("Tine passes this to git and doesn't store it; a credential helper can remember it.")
                    .small()
                    .color(p.text_weak),
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("OK").strong()).clicked() || enter {
                    out = Some(Answer::Value(input.value.clone()));
                }
                if ui.button("Cancel").on_hover_text("Stop the operation").clicked() {
                    out = Some(Answer::Cancel);
                }
            });
        }
    }
    out
}
