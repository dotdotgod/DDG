use ddg_agents_proto::host::TabId;
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_vt::Vt;
use ddg_agents_vt::terminal::{
    MouseAction, MouseButton as VtButton, MouseInput, MouseModes, PasteError, SelectionKind,
};
use gpui::{Bounds, Keystroke, Modifiers, MouseButton, NavigationDirection, Pixels, Point};
use std::time::Duration;

#[derive(Default)]
pub(super) struct ScrollAccumulator {
    remainder_px: f32,
    route: Option<WheelRoute>,
}

impl ScrollAccumulator {
    pub(super) fn reset(&mut self) {
        self.remainder_px = 0.;
    }

    pub(super) fn set_route(&mut self, route: WheelRoute) {
        if self.route != Some(route) {
            self.reset();
            self.route = Some(route);
        }
    }

    pub(super) fn rows(&mut self, delta_y_px: f32, cell_h: f32) -> isize {
        if !delta_y_px.is_finite() || !cell_h.is_finite() || cell_h <= 0. {
            self.reset();
            return 0;
        }
        if delta_y_px == 0. {
            return 0;
        }
        if self.remainder_px.is_sign_negative() != delta_y_px.is_sign_negative() {
            self.reset();
        }
        let total = self.remainder_px + delta_y_px;
        if !total.is_finite() {
            self.reset();
            return 0;
        }
        self.remainder_px = total % cell_h;
        ((total / cell_h).trunc() as isize).saturating_neg()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CellHit {
    pub(super) col: u16,
    pub(super) row: u16,
    pub(super) vertical_outside: isize,
}

fn relative_position(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f32)> {
    let x = f32::from(position.x - bounds.origin.x);
    let y = f32::from(position.y - bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    (x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0. && h > 0.)
        .then_some((x, y))
}

pub(super) fn hit_test_cell(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    cell_w: f32,
    cell_h: f32,
    cols: u16,
    rows: u16,
) -> Option<CellHit> {
    if cols == 0
        || rows == 0
        || !cell_w.is_finite()
        || !cell_h.is_finite()
        || cell_w <= 0.
        || cell_h <= 0.
    {
        return None;
    }
    let (x, y) = relative_position(bounds, position)?;
    let vertical_outside = if y < 0. {
        -1
    } else if y >= f32::from(bounds.size.height) {
        1
    } else {
        0
    };
    Some(CellHit {
        col: ((x / cell_w) as u16).min(cols - 1),
        row: ((y / cell_h) as u16).min(rows - 1),
        vertical_outside,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerRoute {
    App,
    Local,
}

pub(super) fn pointer_route(can_report: bool, tracking: bool, shift: bool) -> PointerRoute {
    if can_report && tracking && !shift {
        PointerRoute::App
    } else {
        PointerRoute::Local
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WheelRoute {
    App,
    Alternate,
    Local,
    Ignore,
}

pub(super) fn wheel_route(can_report: bool, vt: &Vt, shift: bool) -> WheelRoute {
    let tracking = vt.mouse_tracking();
    if pointer_route(can_report, tracking, shift) == PointerRoute::App {
        WheelRoute::App
    } else if !vt.alt_screen() {
        WheelRoute::Local
    } else if can_report && !tracking && !shift && vt.alt_scroll() {
        WheelRoute::Alternate
    } else {
        WheelRoute::Ignore
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct MouseTarget {
    pub(super) host_id: String,
    pub(super) tab_id: TabId,
    pub(super) run_generation: u64,
    pub(super) channel: Option<u32>,
    pub(super) mode: ControlMode,
    pub(super) running: bool,
    pub(super) connected: bool,
    pub(super) can_report: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct GestureModes {
    alt: bool,
    tracking: bool,
    mouse: MouseModes,
}

impl GestureModes {
    pub(super) fn read(vt: &Vt) -> Self {
        Self {
            alt: vt.alt_screen(),
            tracking: vt.mouse_tracking(),
            mouse: vt.mouse_modes(),
        }
    }
}

pub(super) const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) enum TerminalMouseState {
    Selecting {
        target: MouseTarget,
        modes: GestureModes,
        anchor_px: Point<Pixels>,
        anchor_cell: (u16, u16),
        active: bool,
        click_count: usize,
        last_cell: (u16, u16),
    },
    AppMouse {
        target: MouseTarget,
        modes: GestureModes,
        button: MouseButton,
    },
}

impl TerminalMouseState {
    pub(super) fn target(&self) -> &MouseTarget {
        match self {
            Self::Selecting { target, .. } | Self::AppMouse { target, .. } => target,
        }
    }

    pub(super) fn matches(&self, target: &MouseTarget, vt: &Vt) -> bool {
        let modes = match self {
            Self::Selecting { modes, .. } | Self::AppMouse { modes, .. } => modes,
        };
        self.target() == target && *modes == GestureModes::read(vt)
    }

    pub(super) fn start_selection(
        target: MouseTarget,
        vt: &mut Vt,
        position: Point<Pixels>,
        hit: CellHit,
        click_count: usize,
    ) -> Self {
        let kind = match click_count {
            0 | 1 => SelectionKind::Cell,
            2 => SelectionKind::Word,
            _ => SelectionKind::Line,
        };
        vt.select_start(hit.col, hit.row, kind);
        Self::Selecting {
            target,
            modes: GestureModes::read(vt),
            anchor_px: position,
            anchor_cell: (hit.col, hit.row),
            active: false,
            click_count,
            last_cell: (hit.col, hit.row),
        }
    }

    pub(super) fn update_selection(
        &mut self,
        vt: &mut Vt,
        position: Point<Pixels>,
        hit: CellHit,
    ) -> bool {
        let Self::Selecting {
            anchor_px,
            anchor_cell,
            active,
            click_count,
            last_cell,
            ..
        } = self
        else {
            return false;
        };
        let cell = (hit.col, hit.row);
        // A stationary single click stays unselected even if the body moved.
        let still_clicking =
            !*active && *click_count < 2 && (position == *anchor_px || cell == *anchor_cell);
        if still_clicking || cell == *last_cell || !vt.select_update(hit.col, hit.row) {
            return false;
        }
        *last_cell = cell;
        *active = true;
        true
    }

    pub(super) fn autoscroll(&mut self, vt: &mut Vt, hit: CellHit) -> bool {
        let Self::Selecting {
            active, last_cell, ..
        } = self
        else {
            return false;
        };
        if hit.vertical_outside == 0 || vt.alt_screen() {
            return false;
        }
        let offset = vt.scrollbar().offset;
        vt.scroll(hit.vertical_outside.signum());
        if vt.scrollbar().offset == offset {
            return false;
        }
        let (_, rows) = vt.size();
        let row = if hit.vertical_outside < 0 {
            0
        } else {
            rows - 1
        };
        if vt.select_update(hit.col, row) {
            *last_cell = (hit.col, row);
            *active = true;
        }
        true
    }
}

fn vt_button(button: MouseButton) -> VtButton {
    match button {
        MouseButton::Left => VtButton::Left,
        MouseButton::Right => VtButton::Right,
        MouseButton::Middle => VtButton::Middle,
        MouseButton::Navigate(NavigationDirection::Back) => VtButton::Eight,
        MouseButton::Navigate(NavigationDirection::Forward) => VtButton::Nine,
    }
}

fn mouse_input(
    vt: &Vt,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    grid: (f32, f32),
    mods: Modifiers,
    action: MouseAction,
    button: Option<VtButton>,
) -> Option<MouseInput> {
    let (cols, rows) = vt.size();
    let hit = hit_test_cell(bounds, position, grid.0, grid.1, cols, rows)?;
    let (x, y) = relative_position(bounds, position)?;
    let cell_size = (grid.0.ceil() as u32, grid.1.ceil() as u32);
    // Scale subcell pixels into the encoder's integer cell grid, so a
    // fractional font advance doesn't drift into adjacent reported columns.
    let axis = |pos: f32, size: f32, cell: u16, encoded: u32| {
        let fraction = (pos / size - f32::from(cell)).clamp(0., 0.999);
        (f32::from(cell) + fraction) * encoded as f32
    };
    Some(MouseInput {
        action,
        button,
        any_button_pressed: button.is_some() && action != MouseAction::Release,
        shift: mods.shift,
        ctrl: mods.control,
        alt: mods.alt,
        position: (
            axis(x, grid.0, hit.col, cell_size.0),
            axis(y, grid.1, hit.row, cell_size.1),
        ),
        cell_size,
    })
}

pub(super) fn encode_mouse(
    vt: &mut Vt,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    grid: (f32, f32),
    mods: Modifiers,
    action: MouseAction,
    button: Option<MouseButton>,
) -> Option<Vec<u8>> {
    let input = mouse_input(
        vt,
        bounds,
        position,
        grid,
        mods,
        action,
        button.map(vt_button),
    )?;
    vt.encode_mouse(input)
}

pub(super) fn encode_wheel(
    vt: &mut Vt,
    route: WheelRoute,
    rows: isize,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    grid: (f32, f32),
    mods: Modifiers,
) -> Vec<u8> {
    if rows == 0 {
        return Vec::new();
    }
    let up = rows < 0;
    let packet = match route {
        WheelRoute::App => mouse_input(
            vt,
            bounds,
            position,
            grid,
            mods,
            MouseAction::Press,
            Some(if up {
                VtButton::WheelUp
            } else {
                VtButton::WheelDown
            }),
        )
        .and_then(|input| vt.encode_mouse(input)),
        WheelRoute::Alternate => vt.key(
            if up { "up" } else { "down" },
            false,
            mods.control,
            mods.alt,
        ),
        WheelRoute::Local | WheelRoute::Ignore => None,
    };
    // shortcut: cap pathological wheel events to keep one GPUI dispatch bounded;
    // queue/chunk the excess if a platform ever delivers legitimate >256-row events.
    packet.map_or_else(Vec::new, |packet| {
        packet.repeat(rows.unsigned_abs().min(256))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalShortcut {
    Copy,
    Paste,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
}

impl TerminalShortcut {
    pub(super) fn from_keystroke(key: &Keystroke, mac: bool) -> Option<Self> {
        let m = key.modifiers;
        if m.alt || m.function {
            return None;
        }
        if (mac && m.platform && !m.control && !m.shift)
            || (!mac && !m.platform && m.control && m.shift)
        {
            return match key.key.as_str() {
                "c" => Some(Self::Copy),
                "v" => Some(Self::Paste),
                _ => None,
            };
        }
        if !m.shift || m.control || m.platform {
            return None;
        }
        match key.key.as_str() {
            "pageup" => Some(Self::ScrollPageUp),
            "pagedown" => Some(Self::ScrollPageDown),
            "home" => Some(Self::ScrollTop),
            "end" => Some(Self::ScrollBottom),
            _ => None,
        }
    }
}

pub(super) fn copy_selection(vt: &Vt, write: impl FnOnce(String)) -> bool {
    let Some(text) = vt.selected_text() else {
        return false;
    };
    write(text);
    true
}

pub(super) fn scroll_shortcut(vt: &mut Vt, shortcut: TerminalShortcut) -> bool {
    if vt.alt_screen() {
        return false;
    }
    let offset = vt.scrollbar().offset;
    let page = (vt.size().1 as isize).max(1);
    match shortcut {
        TerminalShortcut::ScrollPageUp => vt.scroll(-page),
        TerminalShortcut::ScrollPageDown => vt.scroll(page),
        TerminalShortcut::ScrollTop => vt.scroll_to_top(),
        TerminalShortcut::ScrollBottom => vt.scroll_to_bottom(),
        TerminalShortcut::Copy | TerminalShortcut::Paste => return false,
    }
    vt.scrollbar().offset != offset
}

pub(super) fn return_to_bottom(vt: &mut Vt) -> bool {
    let had_selection = vt.has_selection();
    if had_selection {
        vt.clear_selection();
    }
    let bar = vt.scrollbar();
    let scrolled = bar.offset.saturating_add(bar.len) < bar.total;
    if scrolled {
        vt.scroll_to_bottom();
    }
    had_selection || scrolled
}

pub(super) type InputTarget = (String, TabId, u64, u32);

// Retain only sanitized PTY bytes, never expose them in banner data or Debug.
pub(super) struct PendingPaste {
    target: InputTarget,
    bytes: Vec<u8>,
    pub(super) line_count: usize,
    pub(super) byte_len: usize,
}

impl PendingPaste {
    pub(super) fn matches(&self, target: Option<&InputTarget>, bracketed: bool) -> bool {
        !bracketed && target == Some(&self.target)
    }

    pub(super) fn into_bytes(
        self,
        target: Option<&InputTarget>,
        bracketed: bool,
    ) -> Option<Vec<u8>> {
        self.matches(target, bracketed).then_some(self.bytes)
    }
}

pub(super) enum Paste {
    Ready(Vec<u8>),
    Confirm(PendingPaste),
}

/// No valid focused control target (including any Peek) means zero clipboard reads.
pub(super) fn prepare_paste(
    target: Option<InputTarget>,
    vt: &Vt,
    read: impl FnOnce() -> Option<String>,
) -> Result<Option<Paste>, PasteError> {
    let Some(target) = target else {
        return Ok(None);
    };
    let Some(text) = read() else {
        return Ok(None);
    };
    let bytes = vt.encode_paste(&text)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if !vt.bracketed_paste() {
        // The VT normalized CR/LF/CRLF to one CR for each submitted newline.
        let breaks = bytes.iter().filter(|&&b| b == b'\r').count();
        if breaks > 0 {
            return Ok(Some(Paste::Confirm(PendingPaste {
                target,
                bytes,
                line_count: breaks + 1,
                byte_len: text.len(),
            })));
        }
    }
    Ok(Some(Paste::Ready(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn body() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(160.), px(40.)),
        }
    }

    fn mouse_target() -> MouseTarget {
        MouseTarget {
            host_id: "host".into(),
            tab_id: "tab".into(),
            run_generation: 1,
            channel: Some(2),
            mode: ControlMode::Control,
            running: true,
            connected: true,
            can_report: true,
        }
    }

    #[test]
    fn fractional_wheel_rows_reset_on_direction_change() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(scroll.rows(4., 10.), 0);
        assert_eq!(scroll.rows(4., 10.), 0);
        assert_eq!(scroll.rows(4., 10.), -1);
        assert_eq!(scroll.rows(-8., 10.), 0);
        assert_eq!(scroll.rows(-2., 10.), 1);
        assert_eq!(scroll.rows(f32::NAN, 10.), 0);
        assert_eq!(scroll.rows(1., 0.), 0);
        scroll.set_route(WheelRoute::Local);
        assert_eq!(scroll.rows(8., 10.), 0);
        scroll.set_route(WheelRoute::App);
        assert_eq!(scroll.rows(2., 10.), 0);
    }

    #[test]
    fn hit_test_uses_body_origin_clamps_and_reports_vertical_exit() {
        let hit = |x, y| hit_test_cell(body(), point(px(x), px(y)), 8., 10., 20, 4);
        assert_eq!(
            hit(19., 30.),
            Some(CellHit {
                col: 1,
                row: 1,
                vertical_outside: 0
            })
        );
        assert_eq!(
            hit(3., 0.),
            Some(CellHit {
                col: 0,
                row: 0,
                vertical_outside: -1
            })
        );
        assert_eq!(
            hit(200., 60.),
            Some(CellHit {
                col: 19,
                row: 3,
                vertical_outside: 1
            })
        );
        assert_eq!(hit(f32::INFINITY, 30.), None);
    }

    #[test]
    fn routing_does_not_send_on_readonly_shift_or_disabled_alternate_scroll() {
        let mut vt = Vt::new(20, 4).unwrap();
        let position = point(px(29.), px(31.));
        let mods = Modifiers::default();
        let grid = (8.5, 10.);
        assert_eq!(wheel_route(true, &vt, false), WheelRoute::Local);
        vt.write(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(wheel_route(false, &vt, false), WheelRoute::Local);
        assert_eq!(wheel_route(true, &vt, true), WheelRoute::Local);
        assert!(matches!(
            pointer_route(false, true, false),
            PointerRoute::Local
        ));
        assert!(matches!(
            pointer_route(true, true, true),
            PointerRoute::Local
        ));
        assert_eq!(wheel_route(true, &vt, false), WheelRoute::App);
        assert_eq!(
            encode_mouse(
                &mut vt,
                body(),
                position,
                grid,
                mods,
                MouseAction::Press,
                Some(MouseButton::Left)
            )
            .as_deref(),
            Some(&b"\x1b[<0;3;2M"[..])
        );
        assert_eq!(
            encode_mouse(
                &mut vt,
                body(),
                point(px(500.), px(-20.)),
                grid,
                mods,
                MouseAction::Release,
                Some(MouseButton::Left)
            )
            .as_deref(),
            Some(&b"\x1b[<0;20;1m"[..])
        );
        assert_eq!(
            encode_wheel(&mut vt, WheelRoute::App, -2, body(), position, grid, mods),
            b"\x1b[<64;3;2M\x1b[<64;3;2M"
        );
        vt.write(b"\x1b[?1002l\x1b[?1049h\x1b[?1007l");
        assert_eq!(wheel_route(true, &vt, false), WheelRoute::Ignore);
        assert!(!scroll_shortcut(&mut vt, TerminalShortcut::ScrollTop));
        vt.write(b"\x1b[?1007h\x1b[?1h");
        assert_eq!(wheel_route(false, &vt, false), WheelRoute::Ignore);
        assert_eq!(wheel_route(true, &vt, true), WheelRoute::Ignore);
        assert_eq!(wheel_route(true, &vt, false), WheelRoute::Alternate);
        assert_eq!(
            encode_wheel(
                &mut vt,
                WheelRoute::Alternate,
                2,
                body(),
                position,
                grid,
                mods
            ),
            b"\x1bOB\x1bOB"
        );
    }

    #[test]
    fn gestures_cannot_continue_across_target_permission_or_mode_changes() {
        let mut vt = Vt::new(20, 4).unwrap();
        let target = mouse_target();
        vt.write(b"\x1b[?1002h\x1b[?1006h");
        let gesture = TerminalMouseState::AppMouse {
            target: target.clone(),
            modes: GestureModes::read(&vt),
            button: MouseButton::Left,
        };
        assert!(gesture.matches(&target, &vt));
        assert!(!gesture.matches(
            &MouseTarget {
                run_generation: 2,
                ..target.clone()
            },
            &vt
        ));
        assert!(!gesture.matches(
            &MouseTarget {
                channel: Some(9),
                ..target.clone()
            },
            &vt
        ));
        assert!(!gesture.matches(
            &MouseTarget {
                can_report: false,
                mode: ControlMode::Readonly,
                ..target.clone()
            },
            &vt
        ));
        vt.write(b"\x1b[?1002l\x1b[?1003h");
        assert!(!gesture.matches(&target, &vt));
        vt.write(b"\x1b[?1003l\x1b[?1002h\x1b[?1049h");
        assert!(!gesture.matches(&target, &vt));
    }

    #[test]
    fn local_selection_copies_on_request_and_extends_into_history() {
        let mut vt = Vt::new(20, 2).unwrap();
        vt.write("history\r\n한글 word\r\nlast".as_bytes());
        let target = MouseTarget {
            mode: ControlMode::Readonly,
            can_report: false,
            ..mouse_target()
        };
        let mut copied = None;
        assert!(!copy_selection(&vt, |_| panic!(
            "empty selection must not touch clipboard"
        )));
        let hit = CellHit {
            col: 5,
            row: 0,
            vertical_outside: 0,
        };
        let mut gesture =
            TerminalMouseState::start_selection(target.clone(), &mut vt, body().origin, hit, 2);
        assert!(gesture.matches(&target, &vt));
        assert!(!gesture.matches(
            &MouseTarget {
                running: false,
                ..target.clone()
            },
            &vt
        ));
        assert!(!gesture.matches(
            &MouseTarget {
                connected: false,
                ..target.clone()
            },
            &vt
        ));
        assert!(copy_selection(&vt, |text| copied = Some(text)));
        assert_eq!(copied.as_deref(), Some("word"));
        assert!(!gesture.update_selection(&mut vt, body().origin, hit));
        assert!(gesture.autoscroll(
            &mut vt,
            CellHit {
                col: 0,
                vertical_outside: -1,
                ..hit
            }
        ));
        assert_eq!(vt.scrollbar().offset, 0);
        assert!(vt.selected_text().unwrap().starts_with("history\n"));
        assert!(!gesture.autoscroll(
            &mut vt,
            CellHit {
                vertical_outside: -1,
                ..hit
            }
        ));
        assert!(return_to_bottom(&mut vt));
        assert!(!return_to_bottom(&mut vt));
        let mut gesture =
            TerminalMouseState::start_selection(target, &mut vt, body().origin, hit, 1);
        assert!(!vt.has_selection());
        let moved = CellHit { col: 8, ..hit };
        assert!(!gesture.update_selection(&mut vt, body().origin, moved));
        assert!(gesture.update_selection(&mut vt, point(px(80.), px(21.)), moved));
        assert!(vt.has_selection());
    }

    #[test]
    fn only_explicit_platform_shortcuts_are_intercepted() {
        let shortcut =
            |key: &str, mac| TerminalShortcut::from_keystroke(&Keystroke::parse(key).unwrap(), mac);
        assert_eq!(shortcut("cmd-c", true), Some(TerminalShortcut::Copy));
        assert_eq!(shortcut("cmd-v", true), Some(TerminalShortcut::Paste));
        assert_eq!(
            shortcut("ctrl-shift-c", false),
            Some(TerminalShortcut::Copy)
        );
        assert_eq!(
            shortcut("ctrl-shift-v", false),
            Some(TerminalShortcut::Paste)
        );
        for mac in [false, true] {
            assert_eq!(shortcut("ctrl-c", mac), None);
            assert_eq!(shortcut("ctrl-v", mac), None);
            assert_eq!(
                shortcut("shift-pageup", mac),
                Some(TerminalShortcut::ScrollPageUp)
            );
        }
    }

    #[test]
    fn denied_paste_never_evaluates_clipboard_reader() {
        let vt = Vt::new(20, 4).unwrap();
        // The caller passes no InputTarget for both permission denial and Peek.
        assert!(
            prepare_paste(None, &vt, || panic!("unauthorized clipboard read"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn multiline_paste_requires_same_target_and_mode_at_confirmation() {
        let mut vt = Vt::new(20, 4).unwrap();
        let target = (String::from("host"), TabId::from("tab"), 1, 2);
        assert!(
            matches!(prepare_paste(Some(target.clone()), &vt, || Some("safe".into())), Ok(Some(Paste::Ready(bytes))) if bytes == b"safe")
        );
        let pending = |vt: &Vt| {
            let Some(Paste::Confirm(pending)) =
                prepare_paste(Some(target.clone()), vt, || Some("한\r\nb\rc\n".into())).unwrap()
            else {
                panic!("unbracketed multiline paste must be pending");
            };
            pending
        };
        let paste = pending(&vt);
        assert_eq!((paste.line_count, paste.byte_len), (4, 9));
        assert!(paste.matches(Some(&target), false));
        assert!(
            paste
                .into_bytes(Some(&target), false)
                .is_some_and(|bytes| bytes == "한\rb\rc\r".as_bytes())
        );
        assert!(pending(&vt).into_bytes(None, false).is_none());
        let changed = (target.0.clone(), target.1.clone(), target.2 + 1, target.3);
        assert!(pending(&vt).into_bytes(Some(&changed), false).is_none());
        let paste = pending(&vt);
        vt.write(b"\x1b[?2004h");
        assert!(
            paste
                .into_bytes(Some(&target), vt.bracketed_paste())
                .is_none()
        );
        assert!(
            matches!(prepare_paste(Some(target.clone()), &vt, || Some("a\nb".into())), Ok(Some(Paste::Ready(bytes))) if bytes == b"\x1b[200~a\nb\x1b[201~")
        );
        assert!(matches!(
            prepare_paste(Some(target.clone()), &vt, || Some("\x1b[201~".into())),
            Err(PasteError::Unsafe)
        ));
        assert!(matches!(
            prepare_paste(Some(target), &vt, || Some("a".repeat(64 * 1024 + 1))),
            Err(PasteError::TooLarge)
        ));
    }
}
