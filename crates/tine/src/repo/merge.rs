//! The merge editor in a tab: opening a conflicted file in it, saving the
//! result (only over what was opened), and leaving it with unsaved
//! changes (asked first).

use std::sync::Arc;

use bstr::{BString, ByteSlice as _};
use tine_core::{
    CancelToken,
    merge_file::{self, Unsupported},
};

use super::{Needs, RepoTab, Side, StopOnDrop, View, dialog::Dialog};
use crate::{
    merge_model::{MergeSession, MergeSpans, MergeText},
    worker::{FollowUp, Gen, MergeOpened, MergeSaved, OpKind, OpOutcome},
};

pub enum MergeState {
    Loading,
    /// `unsupported`: the file isn't for this editor (the reason says what
    /// to do instead), rather than something going wrong.
    Failed { message: String, unsupported: bool },
    Ready(Box<MergeSession>),
}

/// A conflicted file in the merge editor.
pub struct MergeEditor {
    pub path: BString,
    seq: Gen,
    /// Stops loading and highlighting once the editor goes.
    _stop: StopOnDrop,
    pub state: MergeState,
    pub saving: bool,
    /// What the last save did (or why the file can't be saved now).
    pub note: Option<String>,
    /// A row to bring into view.
    pub scroll_to: Option<usize>,
    /// Showing (in the diff's place); hidden while another file is looked
    /// at, kept as it is.
    pub shown: bool,
}

impl MergeEditor {
    /// Which opening of the editor this is (saves and edits are for one).
    pub fn id(&self) -> Gen {
        self.seq
    }

    pub fn session(&self) -> Option<&MergeSession> {
        match &self.state {
            MergeState::Ready(s) => Some(s),
            _ => None,
        }
    }

    pub fn session_mut(&mut self) -> Option<&mut MergeSession> {
        match &mut self.state {
            MergeState::Ready(s) => Some(s),
            _ => None,
        }
    }

    pub fn dirty(&self) -> bool {
        self.session().is_some_and(MergeSession::dirty)
    }
}

/// What to do once unsaved merge edits are given up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeLeave {
    Close,
    Open(BString),
    CloseTab,
}

impl RepoTab {
    /// Opens `path` (a conflicted file) in the merge editor, where its
    /// diff would show. Another file's unsaved changes are asked about
    /// first.
    pub fn open_merge_editor(&mut self, path: BString) {
        if let Some(m) = self.merge.as_mut().filter(|m| m.path == path) {
            m.shown = true;
            self.show_merge_file(&path);
            return;
        }
        if self.merge_dirty() {
            self.show_dialog(Dialog::LeaveMerge { then: MergeLeave::Open(path) });
            return;
        }
        self.open_merge_now(path);
    }

    fn open_merge_now(&mut self, path: BString) {
        let cancel = CancelToken::default();
        let seq = self.gens.bump();
        self.worker.merge_open(seq, path.clone(), self.doc_opts, cancel.clone());
        self.merge = Some(MergeEditor {
            path: path.clone(),
            seq,
            _stop: StopOnDrop(cancel),
            state: MergeState::Loading,
            saving: false,
            note: None,
            scroll_to: None,
            shown: true,
        });
        self.show_merge_file(&path);
    }

    /// Selects the file in the list, if it is there (the editor shows in
    /// its diff's place either way).
    fn show_merge_file(&mut self, path: &[u8]) {
        self.clear_browse();
        self.view = View::LocalChanges;
        if let Some(f) = self.list(Side::Unstaged).into_iter().find(|f| f.path == path) {
            self.changes_multi = [f.path.clone()].into();
            if self.changes_selected.as_ref().is_none_or(|(_, s)| s.path != path) {
                self.select_change(Side::Unstaged, f);
            }
        }
    }

    /// The merge editor is showing (also when its file is no longer in the
    /// list: what was resolved can still be copied).
    pub fn merge_shown(&self) -> bool {
        self.merge.as_ref().is_some_and(|m| m.shown)
    }

    /// Another file looked at: the editor (kept as it is) gives it the
    /// place of the diff.
    pub(super) fn merge_file_selected(&mut self, path: &[u8]) {
        if let Some(m) = &mut self.merge {
            m.shown = m.path == path;
        }
    }

    /// Changes not saved: in the editor, or typed into its result.
    pub fn merge_dirty(&self) -> bool {
        self.merge.as_ref().is_some_and(MergeEditor::dirty) || self.merge_draft()
    }

    /// The result is being edited as text, with something typed.
    fn merge_draft(&self) -> bool {
        matches!(&self.dialog, Some(Dialog::EditMergeResult { text, original, .. }) if text != original)
    }

    /// Closes the merge editor (unsaved changes are asked about first).
    pub fn request_close_merge(&mut self) {
        if self.merge_dirty() {
            self.show_dialog(Dialog::LeaveMerge { then: MergeLeave::Close });
        } else {
            self.merge = None;
        }
    }

    /// Unsaved changes were given up: on with what was asked.
    pub fn leave_merge(&mut self, then: MergeLeave) {
        self.merge = None;
        match then {
            MergeLeave::Close => {}
            MergeLeave::Open(path) => self.open_merge_now(path),
            MergeLeave::CloseTab => self.close_requested = true,
        }
    }

    /// Asks to close the tab: `true` if it can go now; otherwise unsaved
    /// merge changes are asked about first (and `close_requested` set once
    /// given up).
    pub fn request_close(&mut self) -> bool {
        if !self.merge_dirty() {
            return true;
        }
        self.show_dialog(Dialog::LeaveMerge { then: MergeLeave::CloseTab });
        false
    }

    pub(super) fn on_merge_opened(&mut self, seq: Gen, result: anyhow::Result<Box<MergeOpened>>) {
        let doc = self.doc_opts;
        let stash = self.stash_conflict.as_ref().map(|c| c.oid);
        let Some(m) = self.merge.as_mut().filter(|m| m.seq == seq) else { return };
        let mut stale = false;
        m.state = match result {
            Ok(mut o) => {
                // A stash Tine applied: say which.
                if let Some(oid) = stash.filter(|_| o.opened.state == tine_core::model::RepoState::Clean) {
                    o.opened.sides.theirs = format!("Stash {} being applied (stage 3)", oid.short());
                    o.opened.sides.ours = "Your files as they were (stage 2)".into();
                }
                let mut session = MergeSession::new(o.opened, o.text, o.doc.tab_width);
                // The display settings changed while it was read.
                if o.doc != doc {
                    session.retext(doc.tab_width);
                    stale = true;
                }
                let first = session.next_unresolved(None, false);
                m.scroll_to = first.map(|k| session.conflict_row[k]);
                let mut session = Box::new(session);
                session.current = first;
                MergeState::Ready(session)
            }
            Err(e) => {
                let unsupported = e.is::<Unsupported>();
                MergeState::Failed { message: format!("{e:#}"), unsupported }
            }
        };
        if stale {
            self.rehighlight_merge();
        }
    }

    pub(super) fn on_merge_highlighted(&mut self, seq: Gen, text: Arc<MergeText>, spans: Arc<MergeSpans>) {
        if !self.doc_opts.highlight {
            return;
        }
        let Some(m) = self.merge.as_mut().filter(|m| m.seq == seq) else { return };
        // Only for the text as it still is.
        if let Some(s) = m.session_mut().filter(|s| Arc::ptr_eq(&s.text, &text)) {
            s.spans = Some(spans);
        }
    }

    /// Colors for the editor's text after it changed.
    fn rehighlight_merge(&mut self) {
        if !self.doc_opts.highlight {
            return;
        }
        let Some(m) = &self.merge else { return };
        if let Some(s) = m.session() {
            let cancel = m._stop.0.clone();
            self.worker.merge_highlight(m.seq, s.text.clone(), m.path.clone(), cancel);
        }
    }

    /// The tab width or highlighting changed.
    pub(super) fn merge_doc_options_changed(&mut self) {
        let tab_width = self.doc_opts.tab_width;
        if let Some(s) = self.merge.as_mut().and_then(MergeEditor::session_mut) {
            s.retext(tab_width);
        }
        self.rehighlight_merge();
    }

    /// Moves to the next (or previous) unresolved conflict.
    pub fn merge_next(&mut self, back: bool) {
        let Some(m) = &mut self.merge else { return };
        let Some(s) = m.session_mut() else { return };
        if let Some(k) = s.next_unresolved(s.current, back) {
            s.current = Some(k);
            m.scroll_to = Some(s.conflict_row[k]);
        }
    }

    /// Opens the result as text to edit (when it can be; not while saving).
    pub fn edit_merge_result(&mut self) {
        let Some(m) = self.merge.as_ref().filter(|m| !m.saving) else { return };
        let Some(s) = m.session() else { return };
        match s.editable_text() {
            Ok(text) => self.show_dialog(Dialog::EditMergeResult { editor: m.id(), original: text.clone(), text }),
            Err(why) => self.error(why.into()),
        }
    }

    /// Takes the result edited as text into the editor it was for.
    pub fn apply_merge_edit(&mut self, editor: Gen, text: &str) {
        match self.merge.as_mut().filter(|m| m.id() == editor).and_then(MergeEditor::session_mut) {
            Some(s) => s.apply_edit(text),
            None => {
                self.error("The merge editor the edited text was for is closed; the text wasn't used".into());
                return;
            }
        }
        self.rehighlight_merge();
    }

    /// Why saving can't happen now (`None`: it can).
    pub fn merge_save_block(&self, resolved: bool) -> Option<String> {
        let m = self.merge.as_ref()?;
        let s = m.session()?;
        if m.saving {
            return Some("Saving…".into());
        }
        if let Some(why) = self.block_reason(Needs::Idle) {
            return Some(why.into());
        }
        if resolved && s.unresolved() > 0 {
            return Some(format!("{} conflict(s) left to resolve", s.unresolved()));
        }
        None
    }

    /// Writes the result over the file (and with `resolved`, marks it
    /// resolved) — only if nothing changed it, its conflict or the
    /// operation since it was opened.
    pub fn save_merge(&mut self, resolved: bool) {
        if self.merge_save_block(resolved).is_some() {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        let Some(m) = &mut self.merge else { return };
        let Some(s) = m.session() else { return };
        let (target, work, result, edit_gen) = (s.opened.target(), s.work.clone(), s.result(), s.edit_gen);
        let editor = m.id();
        m.saving = true;
        m.note = None;
        self.run_op(OpKind::SaveMerge { resolved, editor }, move |git| {
            let saved = merge_file::save(git, &git_dir, &target, &work, &result, resolved)?;
            let name = target.path.to_str_lossy().into_owned();
            let summary = match &saved.staged {
                Some(Ok(())) => format!("Resolved {name}"),
                Some(Err(_)) => format!("Saved {name} (marking it resolved failed)"),
                None => format!("Saved {name}"),
            };
            Ok(OpOutcome::new(summary).with(FollowUp::MergeSaved(Box::new(MergeSaved {
                editor,
                path: target.path,
                edit_gen,
                work: saved.work,
                staged: saved.staged,
            }))))
        });
    }

    /// A save from opening `editor` of the merge editor finished (or
    /// failed: `None`). Another opening (closed and opened again since)
    /// takes nothing from it.
    pub(super) fn merge_save_done(&mut self, editor: Gen, saved: Option<MergeSaved>) {
        let draft = self.merge_draft();
        let Some(m) = self.merge.as_mut().filter(|m| m.id() == editor) else { return };
        m.saving = false;
        let Some(saved) = saved.filter(|s| s.path == m.path) else { return };
        let Some(s) = m.session_mut() else { return };
        s.work = saved.work;
        if s.edit_gen == saved.edit_gen {
            s.saved_gen = saved.edit_gen;
        }
        // Text being edited keeps the editor too.
        let dirty = s.dirty() || draft;
        m.note = Some(match saved.staged {
            // Done with it (unless edited meanwhile).
            Some(Ok(())) if !dirty => {
                self.merge = None;
                return;
            }
            Some(Ok(())) => "Saved and marked resolved; changes made since (or being typed) aren't saved".into(),
            Some(Err(e)) => format!("Saved, but marking it resolved failed: {e}"),
            None => "Saved; the file stays conflicted until marked resolved".into(),
        });
    }

    /// The status changed: an editor whose file isn't conflicted any more
    /// (resolved elsewhere, the operation ended…) closes if nothing is
    /// unsaved, or says so.
    pub(super) fn check_merge_editor(&mut self) {
        let Some(m) = &self.merge else { return };
        let still = self.status.conflicted.iter().any(|f| f.path == m.path);
        if still {
            return;
        }
        if !self.merge_dirty() {
            self.merge = None;
            return;
        }
        if let Some(m) = &mut self.merge
            && !m.saving
        {
            m.note = Some(
                "The file isn't conflicted any more (resolved or ended elsewhere): saving is refused. Copy what you need, \
                 then close the editor."
                    .into(),
            );
        }
    }
}
