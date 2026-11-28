use super::*;
use std::ops::Range;

// Only the preedit belongs to the client. Committed bytes belong to the PTY.
pub(super) struct TerminalInput {
    desktop: WeakEntity<Desktop>,
    pub(super) target: InputTarget,
    pub(super) marked: String,
    selection: Range<usize>,
}

impl Desktop {
    pub(super) fn input_target(&self, window: &Window) -> Option<InputTarget> {
        self.focus
            .is_focused(window)
            .then(|| self.control_target())
            .flatten()
            .map(|(host, tab, run, channel)| (host.to_owned(), tab.to_owned(), run, channel))
    }

    pub(super) fn control_target(&self) -> Option<(&str, &str, u64, u32)> {
        if !self.terminal_has_keyboard() {
            return None;
        }
        self.active_controllable_terminal()
    }

    pub(super) fn file_drop_blocked(&self) -> bool {
        self.peek.is_some() || self.active_overlay.is_some() || self.update_modal.is_some()
    }

    pub(super) fn drop_target(&self, tab_id: &str) -> Option<InputTarget> {
        if self.file_drop_blocked() {
            return None;
        }
        let (host, tab, run, channel) = self.active_controllable_terminal()?;
        (tab == tab_id).then(|| (host.to_owned(), tab.to_owned(), run, channel))
    }

    fn active_controllable_terminal(&self) -> Option<(&str, &str, u64, u32)> {
        let (host, tab) = self.active_terminal_target()?;
        if !self.machine(host)?.is_connected()
            || !tab.is_running()
            || tab.closing
            || tab.mode != ControlMode::Control
        {
            return None;
        }
        Some((host, &tab.id, tab.run_generation, tab.channel?))
    }

    pub(super) fn prepare_terminal_input(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalInput>> {
        let Some(target) = self
            .input_target(window)
            .filter(|_| self.pending_paste.is_none())
        else {
            self.terminal_input = None;
            return None;
        };
        if !self
            .terminal_input
            .as_ref()
            .is_some_and(|i| i.read(cx).target == target)
        {
            let desktop = cx.weak_entity();
            self.terminal_input = Some(cx.new(|_| TerminalInput {
                desktop,
                target,
                marked: String::new(),
                selection: 0..0,
            }));
        }
        self.terminal_input.clone()
    }
}

fn byte_range(s: &str, range: Range<usize>) -> Option<Range<usize>> {
    let offset = |n| {
        let mut len = 0;
        for (index, c) in s.char_indices() {
            if len == n {
                return Some(index);
            }
            len += c.len_utf16();
        }
        (len == n).then_some(s.len())
    };
    (range.start <= range.end).then_some(offset(range.start)?..offset(range.end)?)
}

impl TerminalInput {
    fn replace(&mut self, range: Option<Range<usize>>, text: &str) {
        let bytes = range
            .and_then(|r| byte_range(&self.marked, r))
            .unwrap_or(0..self.marked.len());
        self.marked.replace_range(bytes, text);
    }
}

impl EntityInputHandler for TerminalInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        Some(self.marked[byte_range(&self.marked, range)?].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..self.marked.encode_utf16().count())
    }
    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.marked.is_empty() {
            self.replace_text_in_range(Some(0..0), "", window, cx);
        }
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let had_mark = !self.marked.is_empty();
        self.replace(range, text);
        let committed = std::mem::take(&mut self.marked);
        self.selection = 0..0;
        let input_id = cx.entity_id();
        let _ = self.desktop.update(cx, |desktop, cx| {
            if desktop.input_target(window).as_ref() == Some(&self.target)
                && desktop
                    .terminal_input
                    .as_ref()
                    .is_some_and(|input| input.entity_id() == input_id)
            {
                if !committed.is_empty() {
                    desktop.send_terminal_text(&self.target, committed.as_bytes(), cx);
                }
                if had_mark {
                    cx.notify();
                }
            }
        });
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, text);
        let end = self.marked.encode_utf16().count();
        self.selection = selected.unwrap_or(end..end);
        let _ = self.desktop.update(cx, |_, cx| cx.notify());
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::byte_range;
    #[test]
    fn preedit_ranges_use_utf16() {
        assert_eq!(byte_range("한😀글", 1..3), Some(3..7));
        assert_eq!(byte_range("한😀글", 2..3), None);
    }
}
