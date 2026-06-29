//! View-local pane state owned by the UI thread: scroll offset and selection.
//!
//! [`GridView`] is the half of a pane's state the user drives directly — it is
//! never touched by the server's diff stream and never crosses a thread
//! boundary. It is kept separate from [`GridContent`](super::content::GridContent)
//! so the content can be applied off the UI thread and published as an immutable
//! snapshot while the view stays mutable and responsive on the UI thread
//! (issue #182, §1).

use super::content::{ApplyEffect, ScrollbackFixup};
use super::selection::Selection;

/// View-local state: where the viewport is scrolled and what is selected.
#[derive(Clone, Default)]
pub struct GridView {
    /// Scroll offset from the bottom (0 = live view, >0 = scrolled into history).
    scroll_offset: usize,
    /// Current text selection, if any.
    selection: Option<Selection>,
}

impl GridView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile the view with the content-side consequences of an apply: snap
    /// to the bottom + clear selection on a reset, or shift/clear selection rows
    /// after a scrollback append evicted or added lines.
    pub fn apply_effect(&mut self, effect: ApplyEffect) {
        if effect.reset_view {
            self.scroll_offset = 0;
            self.selection = None;
        }
        match effect.scrollback_fixup {
            Some(ScrollbackFixup::Cleared) => self.selection = None,
            Some(ScrollbackFixup::Shifted { evicted, net }) => {
                if let Some(sel) = &mut self.selection {
                    if evicted > 0 && sel.anchor.row < evicted {
                        self.selection = None;
                    } else {
                        sel.anchor.row = sel.anchor.row.saturating_sub(evicted) + net;
                        sel.end.row = sel.end.row.saturating_sub(evicted) + net;
                    }
                }
            }
            None => {}
        }
    }

    /// Current scroll offset (0 = live, >0 = scrolled into history).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Whether the view is scrolled up from live output.
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Scroll up by `n` display rows, capped at `max_offset` (the content's
    /// total scrollback display rows). Returns whether the offset changed.
    pub fn scroll_up(&mut self, n: usize, max_offset: usize) -> bool {
        let new_offset = (self.scroll_offset + n).min(max_offset);
        let changed = new_offset != self.scroll_offset;
        self.scroll_offset = new_offset;
        changed
    }

    /// Scroll down by `n` rows toward live view. Returns whether it changed.
    pub fn scroll_down(&mut self, n: usize) -> bool {
        let new_offset = self.scroll_offset.saturating_sub(n);
        let changed = new_offset != self.scroll_offset;
        self.scroll_offset = new_offset;
        changed
    }

    /// Snap to the bottom (live view). Returns whether it changed.
    pub fn scroll_to_bottom(&mut self) -> bool {
        let changed = self.scroll_offset > 0;
        self.scroll_offset = 0;
        changed
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    pub fn set_selection(&mut self, sel: Option<Selection>) {
        self.selection = sel;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }
}
