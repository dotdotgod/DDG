//! Native terminal state, rendering snapshot, VT re-serialization (`paint`),
//! binary snapshot encode/decode, and keyboard encoding via Ghostty VT 1.0.1.

use crate::ffi::{
    self, Codepoints, FreeFn, GhosttyString, GridRef, Handle, NewFn, Point, PointCoordinate,
    PointValue, Scroll, ScrollValue, Selection, WriteStop,
};
use std::{ffi::c_void, ops::RangeInclusive, ptr, slice};

pub use crate::ffi::{RowId, Scrollbar};

/// Default RAM scrollback cap per terminal (4 MiB).
pub const DEFAULT_SCROLLBACK_MAX_BYTES: usize = 4 << 20;
/// Default unfinished escape/UTF-8 continuation tracking cap (64 KiB).
pub const DEFAULT_CONTINUATION_MAX_BYTES: usize = 64 * 1024;

struct Owned(Handle, FreeFn);

// Ghostty handles keep no thread-local state and are only accessed via `&mut self`/`&self`.
unsafe impl Send for Owned {}

impl Owned {
    fn create(
        name: &str,
        free: FreeFn,
        init: impl FnOnce(*mut Handle) -> i32,
    ) -> Result<Self, String> {
        let mut handle = ptr::null_mut();
        let result = init(&mut handle);
        if result == ffi::SUCCESS && !handle.is_null() {
            Ok(Self(handle, free))
        } else {
            Err(format!("Ghostty {name} initialization failed ({result})"))
        }
    }

    fn new(name: &str, init: NewFn, free: FreeFn) -> Result<Self, String> {
        Self::create(name, free, |out| unsafe { init(ptr::null(), out) })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0) }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub width: u8,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
}

pub fn style_runs(cells: Vec<Cell>) -> Vec<(Cell, usize)> {
    fn simple(cell: &Cell) -> bool {
        cell.width == 1 && matches!(cell.text.as_bytes(), [b' '..=b'~'])
    }
    let mut runs: Vec<(Cell, usize)> = Vec::new();
    let mut previous_simple = false;
    for cell in cells {
        let mergeable = simple(&cell);
        if mergeable
            && previous_simple
            && let Some((run, columns)) = runs.last_mut()
            && run.fg == cell.fg
            && run.bg == cell.bg
        {
            run.text.push_str(&cell.text);
            *columns += 1;
        } else {
            let columns = usize::from(cell.width);
            runs.push((cell, columns));
        }
        previous_simple = mergeable;
    }
    runs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderDirty {
    Clean,
    Partial,
    Full,
}

pub struct RenderRow {
    pub row: u16,
    pub id: RowId,
    pub selection: Option<RangeInclusive<u16>>,
    pub runs: Vec<(Cell, usize)>,
}

/// A delta against the last successfully applied snapshot of this VT.
pub struct RenderSnapshot {
    pub dirty: RenderDirty,
    pub size: (u16, u16),
    pub rows: Vec<RenderRow>,
    pub cursor: Option<(u16, u16)>,
    pub input_position: Option<(u16, u16)>,
    pub foreground: u32,
    pub background: u32,
}

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Cell = 0,
    Word = 1,
    Line = 2,
}

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

#[repr(i32)]
#[derive(Clone, Copy)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    WheelUp = 4,
    WheelDown = 5,
    WheelLeft = 6,
    WheelRight = 7,
    Eight = 8,
    Nine = 9,
    Ten = 10,
    Eleven = 11,
}

#[derive(Clone, Copy)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub any_button_pressed: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Pixel position relative to the terminal grid origin (no padding).
    pub position: (f32, f32),
    /// Pixel dimensions of one cell (used for both cell and pixel protocols).
    pub cell_size: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseModes {
    pub normal: bool,
    pub button: bool,
    pub any: bool,
    pub sgr: bool,
}

pub const MAX_PASTE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteError {
    TooLarge,
    Unsafe,
    EncodingFailed,
}

pub struct Vt {
    cells: Owned,
    row_iter: Owned,
    render: Owned,
    render_ids: Vec<RowId>,
    render_colors: (u32, u32),
    render_needs_full: bool,
    encoder: Owned,
    mouse_encoder: Owned,
    terminal: Owned,
    // Drop after terminal, which releases the gesture's tracked page refs.
    gesture: Owned,
    selection_kind: SelectionKind,
    pending: Vec<u8>,
}

unsafe extern "C" fn free_gesture(gesture: Handle) {
    unsafe { ffi::ghostty_selection_gesture_free(gesture, ptr::null_mut()) };
}

unsafe extern "C" fn collect_reply(_: Handle, data: Handle, bytes: *const u8, len: usize) {
    if !data.is_null() && !bytes.is_null() && len != 0 && len <= isize::MAX as usize {
        // Ghostty borrows the bytes during the callback. Userdata is set only
        // for the duration of the synchronous call that owns the Vec borrow.
        unsafe { &mut *data.cast::<Vec<u8>>() }
            .extend_from_slice(unsafe { slice::from_raw_parts(bytes, len) });
    }
}

fn rgb_value([r, g, b]: [u8; 3]) -> u32 {
    u32::from_be_bytes([0, r, g, b])
}

fn out<T>(value: &mut T) -> *mut c_void {
    ptr::from_mut(value).cast()
}

fn alloc_bytes(call: impl FnOnce(*mut *mut u8, *mut usize) -> i32) -> Option<Vec<u8>> {
    let mut ptr: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    let rc = call(&mut ptr, &mut len);
    if rc != ffi::SUCCESS {
        return None;
    }
    if ptr.is_null() || len == 0 {
        unsafe { ffi::ghostty_free(ptr::null(), ptr, len) };
        return Some(Vec::new());
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) }.to_vec();
    unsafe { ffi::ghostty_free(ptr::null(), ptr, len) };
    Some(bytes)
}

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Result<Self, String> {
        Self::with_limits(
            cols,
            rows,
            DEFAULT_SCROLLBACK_MAX_BYTES,
            DEFAULT_CONTINUATION_MAX_BYTES,
        )
    }

    pub fn with_limits(
        cols: u16,
        rows: u16,
        scrollback_max_bytes: usize,
        continuation_max_bytes: usize,
    ) -> Result<Self, String> {
        if cols == 0 || rows == 0 {
            return Err("Ghostty terminal dimensions must be nonzero".into());
        }
        let terminal = Owned::create("terminal", ffi::ghostty_terminal_free, |out| unsafe {
            ffi::ghostty_terminal_new(ptr::null(), out, cols, rows)
        })?;
        Self::from_terminal_handle(terminal, scrollback_max_bytes, continuation_max_bytes)
    }

    fn from_terminal_handle(
        terminal: Owned,
        scrollback_max_bytes: usize,
        continuation_max_bytes: usize,
    ) -> Result<Self, String> {
        // Install only the PTY reply callback: no clipboard or other host effects.
        if unsafe {
            ffi::ghostty_terminal_set(
                terminal.0,
                ffi::TERMINAL_OPT_WRITE_PTY,
                collect_reply as *const c_void,
            )
        } != ffi::SUCCESS
        {
            return Err("Ghostty write-PTY callback registration failed".into());
        }
        let _ = unsafe {
            ffi::ghostty_terminal_set(
                terminal.0,
                ffi::TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                (&raw const scrollback_max_bytes).cast(),
            )
        };
        let _ = unsafe {
            ffi::ghostty_terminal_set(
                terminal.0,
                ffi::TERMINAL_OPT_CONTINUATION_MAX_BYTES,
                (&raw const continuation_max_bytes).cast(),
            )
        };
        Ok(Self {
            cells: Owned::new(
                "cells",
                ffi::ghostty_render_state_row_cells_new,
                ffi::ghostty_render_state_row_cells_free,
            )?,
            row_iter: Owned::new(
                "row iterator",
                ffi::ghostty_render_state_row_iterator_new,
                ffi::ghostty_render_state_row_iterator_free,
            )?,
            render: Owned::new(
                "render state",
                ffi::ghostty_render_state_new,
                ffi::ghostty_render_state_free,
            )?,
            encoder: Owned::new(
                "key encoder",
                ffi::ghostty_key_encoder_new,
                ffi::ghostty_key_encoder_free,
            )?,
            render_ids: Vec::new(),
            render_colors: (0, 0),
            render_needs_full: true,
            mouse_encoder: Owned::new(
                "mouse encoder",
                ffi::ghostty_mouse_encoder_new,
                ffi::ghostty_mouse_encoder_free,
            )?,
            terminal,
            gesture: Owned::new(
                "selection gesture",
                ffi::ghostty_selection_gesture_new,
                free_gesture,
            )?,
            selection_kind: SelectionKind::Cell,
            pending: Vec::new(),
        })
    }

    fn capture_replies<R>(&mut self, call: impl FnOnce(Handle) -> R) -> R {
        let terminal = self.terminal.0;
        // USERDATA is a direct pointer, not a pointer to a pointer.
        unsafe {
            assert_eq!(
                ffi::ghostty_terminal_set(
                    terminal,
                    ffi::TERMINAL_OPT_USERDATA,
                    out(&mut self.pending),
                ),
                ffi::SUCCESS
            );
            let result = call(terminal);
            assert_eq!(
                ffi::ghostty_terminal_set(terminal, ffi::TERMINAL_OPT_USERDATA, ptr::null()),
                ffi::SUCCESS
            );
            result
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.capture_replies(|terminal| unsafe {
            ffi::ghostty_terminal_vt_write(terminal, bytes.as_ptr(), bytes.len());
        });
    }

    /// Feeds VT bytes until the parser reaches ground state. Returns the number
    /// of consumed bytes when ground is reached, or `None` if all input was
    /// consumed while still inside an escape sequence or UTF-8 sequence.
    pub fn write_until_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut stop = WriteStop {
            size: size_of::<WriteStop>(),
            consumed: 0,
            reached: 0,
            kind: 0,
            introducer: 0,
            end: 0,
            byte: 0,
        };
        let rc = self.capture_replies(|terminal| unsafe {
            ffi::ghostty_terminal_vt_write_until(
                terminal,
                bytes.as_ptr(),
                bytes.len(),
                ffi::TERMINAL_BOUNDARY_GROUND,
                &mut stop,
            )
        });
        (rc == ffi::SUCCESS && (stop.reached & ffi::TERMINAL_BOUNDARY_GROUND) != 0)
            .then_some(stop.consumed)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.render_needs_full = true;
        self.capture_replies(|terminal| unsafe {
            let result = ffi::ghostty_terminal_resize(terminal, cols, rows, 0, 0);
            if result != ffi::SUCCESS {
                eprintln!("Ghostty terminal resize failed ({result})");
            }
        });
    }

    pub fn size(&self) -> (u16, u16) {
        let (mut cols, mut rows) = (0u16, 0u16);
        unsafe {
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_COLS, out(&mut cols));
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_ROWS, out(&mut rows));
        }
        (cols, rows)
    }

    pub fn alt_screen(&self) -> bool {
        let mut screen = 0i32;
        unsafe {
            ffi::ghostty_terminal_get(
                self.terminal.0,
                ffi::TERMINAL_DATA_ACTIVE_SCREEN,
                out(&mut screen),
            );
        }
        screen == ffi::TERMINAL_SCREEN_ALTERNATE
    }

    pub fn scrollback_rows(&self) -> usize {
        let mut rows: usize = 0;
        unsafe {
            ffi::ghostty_terminal_get(
                self.terminal.0,
                ffi::TERMINAL_DATA_SCROLLBACK_ROWS,
                out(&mut rows),
            );
        }
        rows
    }

    pub fn title(&self) -> String {
        let mut s = GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        if unsafe {
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_TITLE, out(&mut s))
        } != ffi::SUCCESS
            || s.ptr.is_null()
            || s.len == 0
        {
            return String::new();
        }
        String::from_utf8_lossy(unsafe { slice::from_raw_parts(s.ptr, s.len) }).into_owned()
    }

    pub fn replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn pending_replies_len(&self) -> usize {
        self.pending.len()
    }

    pub fn drop_replies_after(&mut self, keep: usize) {
        self.pending.truncate(keep);
    }

    /// Exact unfinished escape/UTF-8 suffix needed to continue parsing seamlessly
    /// after a `paint()`. Returns `None` if the unfinished sequence exceeded
    /// `continuation_max_bytes`.
    pub fn continuation(&self) -> Option<Vec<u8>> {
        alloc_bytes(|out_ptr, out_len| unsafe {
            ffi::ghostty_terminal_continuation_alloc(self.terminal.0, ptr::null(), out_ptr, out_len)
        })
    }

    /// Encodes a complete binary Ghostty 1.0.1 snapshot (both primary and
    /// alternate screens, history, modes, and parser continuation).
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        alloc_bytes(|out_ptr, out_len| unsafe {
            ffi::ghostty_snapshot_encode_alloc(self.terminal.0, ptr::null(), out_ptr, out_len)
        })
    }

    /// Restores a terminal from a Ghostty 1.0.1 binary snapshot.
    pub fn from_snapshot(bytes: &[u8]) -> Option<Self> {
        let decoder = Owned::create(
            "snapshot decoder",
            ffi::ghostty_snapshot_decoder_free,
            |out| unsafe {
                ffi::ghostty_snapshot_decoder_new_buf(ptr::null(), out, bytes.as_ptr(), bytes.len())
            },
        )
        .ok()?;
        let terminal = Owned::create("terminal", ffi::ghostty_terminal_free, |out| unsafe {
            ffi::ghostty_snapshot_decoder_decode(decoder.0, out)
        })
        .ok()?;
        Self::from_terminal_handle(
            terminal,
            DEFAULT_SCROLLBACK_MAX_BYTES,
            DEFAULT_CONTINUATION_MAX_BYTES,
        )
        .ok()
    }

    fn grid_ref(&self, tag: i32, x: u16, y: u32) -> Option<GridRef> {
        let point = Point {
            tag,
            value: PointValue {
                coordinate: PointCoordinate { x, y },
            },
        };
        let mut grid_ref = GridRef {
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        (unsafe { ffi::ghostty_terminal_grid_ref(self.terminal.0, point, &mut grid_ref) }
            == ffi::SUCCESS)
            .then_some(grid_ref)
    }

    fn viewport_ref(&self, col: u16, row: u16) -> Option<GridRef> {
        let (cols, rows) = self.size();
        if col >= cols || row >= rows {
            return None;
        }
        self.grid_ref(ffi::POINT_TAG_VIEWPORT, col, u32::from(row))
    }

    fn install_selection(&mut self, selection: Option<&Selection>) -> bool {
        let value = selection.map_or(ptr::null(), |s| ptr::from_ref(s).cast());
        unsafe {
            ffi::ghostty_terminal_set(self.terminal.0, ffi::TERMINAL_OPT_SELECTION, value)
                == ffi::SUCCESS
        }
    }

    /// Begin at a viewport cell. A cell press by itself clears the selection;
    /// word and line presses select immediately.
    pub fn select_start(&mut self, col: u16, row: u16, kind: SelectionKind) -> bool {
        let Some(cell) = self.viewport_ref(col, row) else {
            return false;
        };
        unsafe { ffi::ghostty_selection_gesture_reset(self.gesture.0, self.terminal.0) };
        self.selection_kind = kind;
        let behaviors = ffi::GestureBehaviors([kind as i32; 3]);
        self.selection_event(
            ffi::GESTURE_PRESS,
            &cell,
            ffi::GESTURE_OPT_BEHAVIORS,
            &behaviors,
        )
    }

    /// Extend to a viewport cell, including both endpoints for a cell drag.
    /// The original anchor is tracked by Ghostty even when the viewport scrolls.
    pub fn select_update(&mut self, col: u16, row: u16) -> bool {
        let Some(end) = self.viewport_ref(col, row) else {
            return false;
        };
        if self.selection_kind == SelectionKind::Cell {
            let mut start = GridRef::default();
            if unsafe {
                ffi::ghostty_selection_gesture_get(
                    self.gesture.0,
                    self.terminal.0,
                    ffi::GESTURE_DATA_ANCHOR,
                    out(&mut start),
                )
            } != ffi::SUCCESS
            {
                return false;
            }
            // Cell coordinates have no subcell pointer position. Select both
            // cells, rather than guessing which side of each cell was hit.
            return self.install_selection(Some(&Selection {
                start,
                end,
                rectangle: false,
            }));
        }
        let (cols, rows) = self.size();
        let geometry = ffi::GestureGeometry {
            columns: u32::from(cols),
            cell_width: 1,
            padding_left: 0,
            screen_height: u32::from(rows),
        };
        self.selection_event(
            ffi::GESTURE_DRAG,
            &end,
            ffi::GESTURE_OPT_GEOMETRY,
            &geometry,
        )
    }

    fn selection_event<T>(&mut self, kind: i32, cell: &GridRef, key: i32, value: &T) -> bool {
        let Ok(event) = Owned::create(
            "selection event",
            ffi::ghostty_selection_gesture_event_free,
            |out| unsafe { ffi::ghostty_selection_gesture_event_new(ptr::null(), out, kind) },
        ) else {
            return false;
        };
        let mut selection = Selection::default();
        let rc = unsafe {
            if ffi::ghostty_selection_gesture_event_set(
                event.0,
                ffi::GESTURE_OPT_REF,
                ptr::from_ref(cell).cast(),
            ) != ffi::SUCCESS
                || ffi::ghostty_selection_gesture_event_set(
                    event.0,
                    key,
                    ptr::from_ref(value).cast(),
                ) != ffi::SUCCESS
            {
                return false;
            }
            ffi::ghostty_selection_gesture_event(
                self.gesture.0,
                self.terminal.0,
                event.0,
                &mut selection,
            )
        };
        match rc {
            ffi::SUCCESS => self.install_selection(Some(&selection)),
            ffi::NO_VALUE if kind == ffi::GESTURE_PRESS => self.install_selection(None),
            _ => false,
        }
    }

    pub fn clear_selection(&mut self) {
        unsafe { ffi::ghostty_selection_gesture_reset(self.gesture.0, self.terminal.0) };
        self.install_selection(None);
    }

    fn current_selection(&self) -> Option<Selection> {
        let mut selection = Selection::default();
        (unsafe {
            ffi::ghostty_terminal_get(
                self.terminal.0,
                ffi::TERMINAL_DATA_SELECTION,
                out(&mut selection),
            )
        } == ffi::SUCCESS)
            .then_some(selection)
    }

    pub fn has_selection(&self) -> bool {
        self.current_selection().is_some()
    }

    pub fn selected_text(&self) -> Option<String> {
        let selection = self.current_selection()?;
        let text = self.format(
            ffi::FORMATTER_FORMAT_PLAIN,
            Some(&selection),
            ffi::FORMATTER_EXTRAS_NONE,
            true,
            true,
        );
        (!text.is_empty()).then(|| String::from_utf8_lossy(&text).into_owned())
    }

    fn format(
        &self,
        emit: i32,
        selection: Option<&Selection>,
        extras: u32,
        unwrap: bool,
        trim: bool,
    ) -> Vec<u8> {
        let Ok(formatter) = Owned::create("formatter", ffi::ghostty_formatter_free, |out| unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), out, self.terminal.0)
        }) else {
            return Vec::new();
        };
        unsafe {
            ffi::ghostty_formatter_set(
                formatter.0,
                ffi::FORMATTER_OPT_EMIT,
                (&raw const emit).cast(),
            );
            ffi::ghostty_formatter_set(
                formatter.0,
                ffi::FORMATTER_OPT_UNWRAP,
                (&raw const unwrap).cast(),
            );
            ffi::ghostty_formatter_set(
                formatter.0,
                ffi::FORMATTER_OPT_TRIM,
                (&raw const trim).cast(),
            );
            ffi::ghostty_formatter_set(
                formatter.0,
                ffi::FORMATTER_OPT_EXTRAS,
                (&raw const extras).cast(),
            );
            if let Some(sel) = selection
                && ffi::ghostty_formatter_set(
                    formatter.0,
                    ffi::FORMATTER_OPT_SELECTION,
                    ptr::from_ref(sel).cast(),
                ) != ffi::SUCCESS
            {
                return Vec::new();
            }
        }
        alloc_bytes(|out_ptr, out_len| unsafe {
            ffi::ghostty_formatter_format_alloc(formatter.0, ptr::null(), out_ptr, out_len)
        })
        .unwrap_or_default()
    }

    /// Plain text of the active screen and scrollback (for inspection & tests).
    pub fn plain_text(&self) -> String {
        String::from_utf8_lossy(&self.format(
            ffi::FORMATTER_FORMAT_PLAIN,
            None,
            ffi::FORMATTER_EXTRAS_NONE,
            true,
            true,
        ))
        .into_owned()
    }

    /// Produces a self-contained VT byte sequence that recreates up to
    /// `history_lines` of scrollback, the active screen (including alternate
    /// screen restoration), cursor, styles, modes, title, and any unfinished
    /// parser continuation so subsequent live PTY bytes continue seamlessly.
    pub fn paint(&self, history_lines: usize) -> Vec<u8> {
        let mut out = vec![0x18]; // CAN: cancel any half-finished sequence on the receiver.
        out.extend(self.paint_body(history_lines));
        if let Some(cont) = self.continuation() {
            out.extend(cont);
        }
        out
    }

    fn paint_body(&self, history_lines: usize) -> Vec<u8> {
        let (cols, rows) = self.size();
        if cols == 0 || rows == 0 {
            return Vec::new();
        }
        let last = (cols.saturating_sub(1), rows.saturating_sub(1) as u32);
        // Reset parser modes that soft reset leaves untouched so stale viewer
        // modes never leak across reconnects or resyncs.
        let mut out = b"\x1b[!p\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?1l\x1b[?66l\x1b[?2031l\x1b[?2048l\x1b[=0;1u\
\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[H\x1b[2J"
            .to_vec();

        if self.alt_screen()
            && let Some(snap) = self.snapshot()
            && let Some(mut primary) = Self::from_snapshot(&snap)
        {
            primary.write(b"\x18\x1b[?1049l");
            out = primary.paint_body(history_lines);
        }

        let available = self.scrollback_rows();
        let start = if self.alt_screen() {
            out.extend_from_slice(b"\x1b[?1049h\x1b[!p");
            self.grid_ref(ffi::POINT_TAG_ACTIVE, 0, 0)
        } else if history_lines > 0 && available > 0 {
            self.grid_ref(
                ffi::POINT_TAG_HISTORY,
                0,
                available.saturating_sub(history_lines) as u32,
            )
        } else {
            self.grid_ref(ffi::POINT_TAG_ACTIVE, 0, 0)
        };

        if let Some(start) = start
            && let Some(end) = self.grid_ref(ffi::POINT_TAG_ACTIVE, last.0, last.1)
        {
            // Format history and active rows together so replay preserves wraps
            // through their boundary, rather than inserting a hard newline.
            let sel = Selection {
                start,
                end,
                rectangle: false,
            };
            let extras = ffi::FORMATTER_EXTRAS_ALL & !ffi::FORMATTER_EXTRA_PALETTE;
            let screen = self.format(
                ffi::FORMATTER_FORMAT_VT,
                Some(&sel),
                extras,
                self.mode(ffi::MODE_AUTO_WRAP),
                false,
            );
            // Synchronized output would freeze a fresh viewer; alt-screen
            // transitions are already emitted explicitly above.
            out.extend(strip_sequences(
                &screen,
                &[b"\x1b[?2026h", b"\x1b[?47h", b"\x1b[?1047h", b"\x1b[?1049h"],
            ));
        }

        // Encode this explicitly even when it is at Ghostty's default:
        // a resync can target a viewer with a different current setting.
        out.extend_from_slice(if self.alt_scroll() {
            b"\x1b[?1007h"
        } else {
            b"\x1b[?1007l"
        });

        let title = self.title();
        if !title.is_empty() {
            out.extend_from_slice(format!("\x1b]2;{title}\x1b\\").as_bytes());
        }
        out
    }

    pub fn pwd(&self) -> Option<String> {
        let mut pwd = GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        if unsafe {
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_PWD, out(&mut pwd))
        } != ffi::SUCCESS
            || pwd.ptr.is_null()
            || !(1..=8192).contains(&pwd.len)
        {
            return None;
        }
        let raw = std::str::from_utf8(unsafe { slice::from_raw_parts(pwd.ptr, pwd.len) }).ok()?;
        let path = if let Some(uri) = raw.strip_prefix("file://") {
            &uri[uri.find('/')?..]
        } else {
            raw
        };
        let mut bytes = Vec::with_capacity(path.len());
        if raw.starts_with("file://") {
            let mut input = path.bytes();
            while let Some(byte) = input.next() {
                bytes.push(if byte == b'%' {
                    let hi = char::from(input.next()?).to_digit(16)?;
                    let lo = char::from(input.next()?).to_digit(16)?;
                    (hi * 16 + lo) as u8
                } else {
                    byte
                });
            }
        } else {
            bytes.extend_from_slice(path.as_bytes());
        }
        let decoded = String::from_utf8(bytes).ok()?;
        (!decoded.is_empty()).then_some(decoded)
    }

    pub fn directory_name(&self) -> Option<String> {
        let decoded = self.pwd()?;
        let part = decoded.trim_end_matches('/').rsplit('/').next()?;
        let name: String = part
            .chars()
            .filter(|c| {
                !c.is_control()
                    && !matches!(c, '/' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            .take(80)
            .collect();
        (!name.trim().is_empty()).then_some(name)
    }

    pub fn frame(&mut self) -> (Vec<Vec<Cell>>, Option<(u16, u16)>) {
        // Legacy full extraction is not an acknowledgement of a delta cache.
        self.render_needs_full = true;
        if !self.refresh_render() {
            return (Vec::new(), None);
        }
        let rows = (|| {
            self.begin_rows()?;
            let mut rows = Vec::new();
            let mut buf = vec![0; 16];
            while unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iter.0) } {
                rows.push(
                    self.viewport_row(&mut buf, false)?
                        .into_iter()
                        .map(|(c, _)| c)
                        .collect(),
                );
            }
            Some(rows)
        })();
        (rows.unwrap_or_default(), self.viewport_cursor())
    }

    /// Apply a delta to one cache per VT. Pass `force_full` when that cache
    /// is cleared/replaced, or its shaping inputs (font, scale, theme) change.
    /// Return true from `apply` only after all rows and metadata are applied.
    /// Returns `Some(true)` on commit, `Some(false)` on rejection, and `None`
    /// on a Ghostty error. Rejection, error or unwinding forces a full retry.
    pub fn update_render(
        &mut self,
        force_full: bool,
        apply: impl FnOnce(RenderSnapshot) -> bool,
    ) -> Option<bool> {
        let force_full = force_full || self.render_needs_full;
        self.render_needs_full = true;
        if !self.refresh_render() {
            return None;
        }
        let (cols, rows) = self.size();
        let state = self.render.0;
        let mut raw_dirty = -1i32;
        if unsafe {
            ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_DIRTY, out(&mut raw_dirty))
        } != ffi::SUCCESS
        {
            return None;
        }
        let mut dirty = match raw_dirty {
            ffi::RENDER_DIRTY_FALSE => RenderDirty::Clean,
            ffi::RENDER_DIRTY_PARTIAL => RenderDirty::Partial,
            ffi::RENDER_DIRTY_FULL => RenderDirty::Full,
            _ => return None,
        };

        // Even a clean row's identity can change when Ghostty reallocates its
        // pages. Never reuse a cached row at an unverified viewport index.
        self.begin_rows()?;
        let iter = self.row_iter.0;
        let mut ids = Vec::with_capacity(usize::from(rows));
        let mut changed = Vec::new();
        while unsafe { ffi::ghostty_render_state_row_iterator_next(iter) } {
            let mut id = RowId::default();
            let mut row_dirty = false;
            unsafe {
                if ffi::ghostty_render_state_row_get(iter, ffi::RENDER_ROW_DATA_ID, out(&mut id))
                    != ffi::SUCCESS
                {
                    id = RowId::default();
                }
                if ffi::ghostty_render_state_row_get(
                    iter,
                    ffi::RENDER_ROW_DATA_DIRTY,
                    out(&mut row_dirty),
                ) != ffi::SUCCESS
                {
                    return None;
                }
            }
            if row_dirty {
                changed.push(ids.len());
            }
            ids.push(id);
        }
        if ids.len() != usize::from(rows) {
            return None;
        }
        let default_color = |kind| {
            let mut rgb = [0u8; 3];
            (unsafe { ffi::ghostty_render_state_get(state, kind, out(&mut rgb)) } == ffi::SUCCESS)
                .then(|| rgb_value(rgb))
        };
        let colors = (
            default_color(ffi::RENDER_DATA_COLOR_FOREGROUND)?,
            default_color(ffi::RENDER_DATA_COLOR_BACKGROUND)?,
        );
        if force_full
            || colors != self.render_colors
            || ids != self.render_ids
            || ids.contains(&RowId::default())
        {
            dirty = RenderDirty::Full;
        }

        let mut updates = Vec::new();
        if dirty != RenderDirty::Clean {
            self.begin_rows()?;
            let mut buf = vec![0; 16];
            let mut changed = changed.into_iter().peekable();
            for (row, &id) in ids.iter().enumerate() {
                if !unsafe { ffi::ghostty_render_state_row_iterator_next(iter) } {
                    return None;
                }
                if dirty == RenderDirty::Partial && changed.next_if_eq(&row).is_none() {
                    continue;
                }
                let mut range = ffi::RowSelection {
                    size: size_of::<ffi::RowSelection>(),
                    start_x: 0,
                    end_x: 0,
                };
                let selection = match unsafe {
                    ffi::ghostty_render_state_row_get(
                        iter,
                        ffi::RENDER_ROW_DATA_SELECTION,
                        out(&mut range),
                    )
                } {
                    ffi::SUCCESS => Some(range.start_x..=range.end_x),
                    ffi::NO_VALUE => None,
                    _ => return None,
                };
                updates.push(RenderRow {
                    row: row as u16,
                    id,
                    selection,
                    runs: self.viewport_row(&mut buf, true)?,
                });
            }
        }
        if !apply(RenderSnapshot {
            dirty,
            size: (cols, rows),
            rows: updates,
            cursor: self.viewport_cursor(),
            input_position: self.input_position(),
            foreground: colors.0,
            background: colors.1,
        }) {
            return Some(false);
        }
        if unsafe { ffi::ghostty_render_state_clean(state) } != ffi::SUCCESS {
            return None;
        }
        self.render_ids = ids;
        self.render_colors = colors;
        self.render_needs_full = false;
        Some(true)
    }

    fn refresh_render(&mut self) -> bool {
        unsafe { ffi::ghostty_render_state_update(self.render.0, self.terminal.0) == ffi::SUCCESS }
    }

    fn begin_rows(&mut self) -> Option<()> {
        (unsafe {
            ffi::ghostty_render_state_get(
                self.render.0,
                ffi::RENDER_DATA_ROW_ITERATOR,
                out(&mut self.row_iter.0),
            )
        } == ffi::SUCCESS)
            .then_some(())
    }

    fn viewport_row(
        &mut self,
        buf: &mut Vec<u32>,
        merge_ascii: bool,
    ) -> Option<Vec<(Cell, usize)>> {
        let mut line: Vec<(Cell, usize)> = Vec::new();
        let mut previous_simple = false;
        unsafe {
            if ffi::ghostty_render_state_row_get(
                self.row_iter.0,
                ffi::RENDER_ROW_DATA_CELLS,
                out(&mut self.cells.0),
            ) != ffi::SUCCESS
            {
                return None;
            }
            let cells = self.cells.0;
            while ffi::ghostty_render_state_row_cells_next(cells) {
                let (mut raw, mut wide) = (0u64, 0i32);
                if ffi::ghostty_render_state_row_cells_get(
                    cells,
                    ffi::RENDER_CELL_DATA_RAW,
                    out(&mut raw),
                ) != ffi::SUCCESS
                    || ffi::ghostty_cell_get(raw, ffi::CELL_DATA_WIDE, out(&mut wide))
                        != ffi::SUCCESS
                {
                    return None;
                }
                let width = match wide {
                    0 => 1,
                    1 => 2,
                    2 | 3 => continue, // Wide-character spacer cells carry no text.
                    _ => return None,
                };
                let graphemes = |buf: &mut [u32]| {
                    let mut points = Codepoints {
                        ptr: buf.as_mut_ptr(),
                        cap: buf.len(),
                        len: 0,
                    };
                    let rc = ffi::ghostty_render_state_row_cells_get(
                        cells,
                        ffi::RENDER_CELL_DATA_GRAPHEMES,
                        out(&mut points),
                    );
                    (rc, points.len)
                };
                let (mut result, mut len) = graphemes(buf);
                if result == ffi::OUT_OF_SPACE && len > buf.len() {
                    buf.try_reserve(len - buf.len()).ok()?;
                    buf.resize(len, 0);
                    (result, len) = graphemes(buf);
                }
                if result != ffi::SUCCESS {
                    return None;
                }
                let points = buf.get(..len)?;
                let simple = match points {
                    [] => Some(' '),
                    [cp @ 32..=126] => char::from_u32(*cp),
                    _ => None,
                }
                .filter(|_| merge_ascii && width == 1);
                let color = |kind| {
                    let mut rgb = [0u8; 3];
                    match ffi::ghostty_render_state_row_cells_get(cells, kind, out(&mut rgb)) {
                        ffi::SUCCESS => Some(Some(rgb_value(rgb))),
                        ffi::NO_VALUE => Some(None),
                        _ => None,
                    }
                };
                let (fg, bg) = (
                    color(ffi::RENDER_CELL_DATA_FG_COLOR)?,
                    color(ffi::RENDER_CELL_DATA_BG_COLOR)?,
                );
                if let Some(ch) = simple
                    && previous_simple
                    && let Some((run, columns)) = line.last_mut()
                    && run.fg == fg
                    && run.bg == bg
                {
                    run.text.push(ch);
                    *columns += 1;
                } else {
                    let text = if points.is_empty() {
                        " ".into()
                    } else {
                        points
                            .iter()
                            .map(|&cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
                            .collect()
                    };
                    line.push((
                        Cell {
                            text,
                            width,
                            fg,
                            bg,
                        },
                        usize::from(width),
                    ));
                }
                previous_simple = simple.is_some();
            }
        }
        Some(line)
    }

    /// Zero-based viewport (column, row), or `None` when hidden or offscreen.
    fn viewport_cursor(&self) -> Option<(u16, u16)> {
        let state = self.render.0;
        let mut visible = false;
        unsafe {
            if ffi::ghostty_render_state_get(
                state,
                ffi::RENDER_DATA_CURSOR_VISIBLE,
                out(&mut visible),
            ) != ffi::SUCCESS
                || !visible
            {
                return None;
            }
        }
        self.input_position()
    }

    /// Insertion point in the last rendered viewport, even if a TUI hides the cursor.
    pub fn input_position(&self) -> Option<(u16, u16)> {
        let state = self.render.0;
        let mut present = false;
        let (mut x, mut y, mut cols, mut rows) = (0u16, 0u16, 0u16, 0u16);
        unsafe {
            if ffi::ghostty_render_state_get(
                state,
                ffi::RENDER_DATA_CURSOR_IN_VIEWPORT,
                out(&mut present),
            ) != ffi::SUCCESS
                || !present
                || ffi::ghostty_render_state_get(
                    state,
                    ffi::RENDER_DATA_CURSOR_VIEWPORT_X,
                    out(&mut x),
                ) != ffi::SUCCESS
                || ffi::ghostty_render_state_get(
                    state,
                    ffi::RENDER_DATA_CURSOR_VIEWPORT_Y,
                    out(&mut y),
                ) != ffi::SUCCESS
                || ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_COLS, out(&mut cols))
                    != ffi::SUCCESS
                || ffi::ghostty_render_state_get(state, ffi::RENDER_DATA_ROWS, out(&mut rows))
                    != ffi::SUCCESS
            {
                return None;
            }
        }
        (x < cols && y < rows).then_some((x, y))
    }

    pub fn scrollbar(&self) -> Scrollbar {
        let mut bar = Scrollbar::default();
        unsafe {
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_SCROLLBAR, out(&mut bar));
        }
        bar
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_viewport(ffi::SCROLL_VIEWPORT_TOP, 0);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_viewport(ffi::SCROLL_VIEWPORT_BOTTOM, 0);
    }

    /// Move the viewport by rows; negative values scroll back in history.
    pub fn scroll(&mut self, delta: isize) {
        if delta != 0 {
            self.scroll_viewport(ffi::SCROLL_VIEWPORT_DELTA, delta);
        }
    }

    fn scroll_viewport(&mut self, tag: i32, delta: isize) {
        let mut value = ScrollValue { padding: [0; 2] };
        value.delta = delta;
        let result = unsafe {
            ffi::ghostty_terminal_scroll_viewport(self.terminal.0, Scroll { tag, value })
        };
        if result != ffi::SUCCESS {
            eprintln!("Ghostty viewport scroll failed ({result})");
        }
    }

    pub fn mouse_tracking(&self) -> bool {
        let mut enabled = false;
        (unsafe {
            ffi::ghostty_terminal_get(
                self.terminal.0,
                ffi::TERMINAL_DATA_MOUSE_TRACKING,
                out(&mut enabled),
            )
        } == ffi::SUCCESS)
            && enabled
    }

    fn mode(&self, mode: u16) -> bool {
        // In VT 1.0.1 mode queries are a terminal data key, not a mode_get symbol.
        let mut config = ffi::ModeConfig { mode, value: false };
        (unsafe {
            ffi::ghostty_terminal_get(self.terminal.0, ffi::TERMINAL_DATA_MODE, out(&mut config))
        } == ffi::SUCCESS)
            && config.value
    }

    pub fn mouse_modes(&self) -> MouseModes {
        MouseModes {
            normal: self.mode(ffi::MODE_MOUSE_NORMAL),
            button: self.mode(ffi::MODE_MOUSE_BUTTON),
            any: self.mode(ffi::MODE_MOUSE_ANY),
            sgr: self.mode(ffi::MODE_MOUSE_SGR),
        }
    }

    pub fn alt_scroll(&self) -> bool {
        self.mode(ffi::MODE_ALT_SCROLL)
    }

    /// Changes the renderer's fallback colors without overwriting OSC 10/11 overrides.
    pub fn set_default_colors(&mut self, foreground: u32, background: u32) -> bool {
        [
            (ffi::TERMINAL_OPT_COLOR_FOREGROUND, foreground),
            (ffi::TERMINAL_OPT_COLOR_BACKGROUND, background),
        ]
        .into_iter()
        .all(|(option, color)| {
            let [_, red, green, blue] = color.to_be_bytes();
            let rgb = [red, green, blue];
            unsafe {
                ffi::ghostty_terminal_set(self.terminal.0, option, rgb.as_ptr().cast())
                    == ffi::SUCCESS
            }
        })
    }

    pub fn bracketed_paste(&self) -> bool {
        self.mode(ffi::MODE_BRACKETED_PASTE)
    }

    /// Return PTY input, or None for events the active protocol does not report.
    pub fn encode_mouse(&mut self, input: MouseInput) -> Option<Vec<u8>> {
        let (x, y) = input.position;
        let (cell_width, cell_height) = input.cell_size;
        if !self.mouse_tracking()
            // SGR-pixel reports convert the coordinates to signed C integers.
            || !((i32::MIN as f32)..(i32::MAX as f32)).contains(&x)
            || !((i32::MIN as f32)..(i32::MAX as f32)).contains(&y)
            || cell_width == 0
            || cell_height == 0
            || (input.action != MouseAction::Motion && input.button.is_none())
        {
            return None;
        }
        let (cols, rows) = self.size();
        let size = ffi::MouseSize {
            size: size_of::<ffi::MouseSize>(),
            screen_width: u32::from(cols).checked_mul(cell_width)?,
            screen_height: u32::from(rows).checked_mul(cell_height)?,
            cell_width,
            cell_height,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let event = Owned::new(
            "mouse event",
            ffi::ghostty_mouse_event_new,
            ffi::ghostty_mouse_event_free,
        )
        .ok()?;
        let encoder = self.mouse_encoder.0;
        let mods = input_mods(input.shift, input.ctrl, input.alt);
        let mut bytes = [0; 128];
        let mut len = 0;
        unsafe {
            if ffi::ghostty_mouse_encoder_set_from_terminal(encoder, self.terminal.0)
                != ffi::SUCCESS
                || ffi::ghostty_mouse_encoder_set(
                    encoder,
                    ffi::MOUSE_OPT_SIZE,
                    ptr::from_ref(&size).cast(),
                ) != ffi::SUCCESS
                || ffi::ghostty_mouse_encoder_set(
                    encoder,
                    ffi::MOUSE_OPT_ANY_BUTTON_PRESSED,
                    ptr::from_ref(&input.any_button_pressed).cast(),
                ) != ffi::SUCCESS
                || ffi::ghostty_mouse_event_set_action(event.0, input.action as i32) != ffi::SUCCESS
                || input.button.is_some_and(|button| {
                    ffi::ghostty_mouse_event_set_button(event.0, button as i32) != ffi::SUCCESS
                })
                || ffi::ghostty_mouse_event_set_mods(event.0, mods) != ffi::SUCCESS
                || ffi::ghostty_mouse_event_set_position(event.0, ffi::MousePosition { x, y })
                    != ffi::SUCCESS
                || ffi::ghostty_mouse_encoder_encode(
                    encoder,
                    event.0,
                    bytes.as_mut_ptr(),
                    bytes.len(),
                    &mut len,
                ) != ffi::SUCCESS
                || len == 0
            {
                return None;
            }
        }
        Some(bytes.get(..len)?.to_vec())
    }

    /// Encode independently of PTY-query replies. The caller must confirm
    /// unbracketed pastes containing line breaks before sending the bytes.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, PasteError> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(PasteError::TooLarge);
        }
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let bracketed = self.bracketed_paste();
        if bracketed && (text.contains("\x1b[201~") || text.contains("\u{9b}201~")) {
            return Err(PasteError::Unsafe);
        }
        // Ghostty handles framing and LF -> CR. Collapse CRLF first so it
        // does not turn into two Enters; also block C1 and remaining C0 controls.
        let normalized;
        let text = if !bracketed && text.contains("\r\n") {
            normalized = text.replace("\r\n", "\n");
            &normalized
        } else {
            text
        };
        let clean: String = text
            .chars()
            .map(|ch| {
                if ch.is_control() && !matches!(ch, '\t' | '\r' | '\n') {
                    ' '
                } else {
                    ch
                }
            })
            .collect();
        let mut data = clean.into_bytes();
        let mut bytes = vec![0; data.len() + 12];
        let mut len = 0;
        if unsafe {
            ffi::ghostty_paste_encode(
                data.as_mut_ptr().cast(),
                data.len(),
                bracketed,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut len,
            )
        } != ffi::SUCCESS
            || len > bytes.len()
        {
            return Err(PasteError::EncodingFailed);
        }
        bytes.truncate(len);
        Ok(bytes)
    }

    pub fn key(&mut self, key: &str, shift: bool, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
        let (code, unshifted, text) = key_event(key, shift)?;
        let event = Owned::new(
            "key event",
            ffi::ghostty_key_event_new,
            ffi::ghostty_key_event_free,
        )
        .ok()?;
        let mods = input_mods(shift, ctrl, alt);
        let (encoder, event_handle) = (self.encoder.0, event.0);
        let mut bytes = vec![0; 32];
        let mut len = 0;
        unsafe {
            if ffi::ghostty_key_encoder_set_from_terminal(encoder, self.terminal.0) != ffi::SUCCESS
                || ffi::ghostty_key_event_set_action(event_handle, 1) != ffi::SUCCESS
                || ffi::ghostty_key_event_set_key(event_handle, code) != ffi::SUCCESS
                || ffi::ghostty_key_event_set_mods(event_handle, mods) != ffi::SUCCESS
                || ffi::ghostty_key_event_set_unshifted_codepoint(event_handle, unshifted)
                    != ffi::SUCCESS
                || ffi::ghostty_key_event_set_utf8(
                    event_handle,
                    GhosttyString {
                        ptr: text.as_ptr(),
                        len: text.len(),
                    },
                ) != ffi::SUCCESS
            {
                return None;
            }
            let mut result = ffi::ghostty_key_encoder_encode(
                encoder,
                event_handle,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut len,
            );
            if result == ffi::OUT_OF_SPACE && len > bytes.len() {
                bytes.try_reserve(len - bytes.len()).ok()?;
                bytes.resize(len, 0);
                result = ffi::ghostty_key_encoder_encode(
                    encoder,
                    event_handle,
                    bytes.as_mut_ptr(),
                    bytes.len(),
                    &mut len,
                );
            }
            if result != ffi::SUCCESS || len == 0 || len > bytes.len() {
                return None;
            }
        }
        bytes.truncate(len);
        Some(bytes)
    }

    #[cfg(test)]
    pub(crate) fn lines(&mut self) -> Vec<String> {
        self.frame()
            .0
            .into_iter()
            .map(|row| {
                let text: String = row.into_iter().map(|cell| cell.text).collect();
                text.trim_end_matches(' ').to_owned()
            })
            .collect()
    }
}

fn strip_sequences(bytes: &[u8], needles: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(needle) = needles.iter().find(|n| bytes[i..].starts_with(n)) {
            i += needle.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn input_mods(shift: bool, ctrl: bool, alt: bool) -> u16 {
    u16::from(shift) | (u16::from(ctrl) << 1) | (u16::from(alt) << 2)
}

fn key_event(key: &str, shift: bool) -> Option<(i32, u32, String)> {
    let name = key.to_ascii_lowercase();
    let code = match name.as_str() {
        "backspace" => 53,
        "enter" | "return" => 58,
        "space" => 63,
        "tab" => 64,
        "delete" => 68,
        "end" => 69,
        "home" => 71,
        "insert" => 72,
        "pagedown" | "page_down" => 73,
        "pageup" | "page_up" => 74,
        "down" | "arrowdown" => 75,
        "left" | "arrowleft" => 76,
        "right" | "arrowright" => 77,
        "up" | "arrowup" => 78,
        "escape" | "esc" => 120,
        _ => {
            if let Some(n) = name.strip_prefix('f').and_then(|s| s.parse::<i32>().ok())
                && (1..=25).contains(&n)
            {
                return Some((120 + n, 0, String::new()));
            }
            let mut chars = key.chars();
            let ch = chars.next()?;
            if ch.is_control() || chars.next().is_some() {
                return None;
            }
            if ch.is_ascii_alphabetic() {
                let base = ch.to_ascii_lowercase();
                let text = if shift { ch.to_ascii_uppercase() } else { ch };
                return Some((
                    20 + (base as i32 - 'a' as i32),
                    base as u32,
                    text.to_string(),
                ));
            }
            const BASE: &str = "`\\[],0123456789=-.';/ ";
            const SHIFT: &str = "~|{}<)!@#$%^&*(+_>\":? ";
            const CODES: [i32; 22] = [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 46, 47, 48, 49, 50, 63,
            ];
            if let Some(i) = BASE.find(ch).or_else(|| SHIFT.find(ch)) {
                let text = if shift {
                    SHIFT.as_bytes()[i] as char
                } else {
                    ch
                };
                return Some((CODES[i], BASE.as_bytes()[i] as u32, text.to_string()));
            }
            return Some((0, ch as u32, ch.to_string()));
        }
    };
    Some(if code == 63 {
        (code, 32, " ".into())
    } else {
        (code, 0, String::new())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(vt: &mut Vt, force_full: bool) -> RenderSnapshot {
        let mut snapshot = None;
        assert_eq!(
            vt.update_render(force_full, |next| {
                snapshot = Some(next);
                true
            }),
            Some(true)
        );
        snapshot.unwrap()
    }

    fn check_cache(delta: RenderSnapshot, full: RenderSnapshot, cache: &mut Vec<RenderRow>) {
        assert_eq!(full.dirty, RenderDirty::Full);
        assert_eq!(delta.size, full.size);
        assert_eq!(delta.cursor, full.cursor);
        assert_eq!(delta.input_position, full.input_position);
        assert_eq!(delta.foreground, full.foreground);
        assert_eq!(delta.background, full.background);
        if delta.dirty == RenderDirty::Full {
            *cache = delta.rows;
        } else {
            for row in delta.rows {
                let index = usize::from(row.row);
                assert_eq!(row.id, cache[index].id);
                cache[index] = row;
            }
        }
        assert_eq!(cache.len(), full.rows.len());
        for (got, expected) in cache.iter().zip(full.rows) {
            assert_eq!(got.row, expected.row);
            assert_eq!(got.selection, expected.selection);
            assert_eq!(got.runs, expected.runs);
        }
    }

    #[test]
    fn dirty_cache_matches_full_refresh_oracle() {
        let mut vt = Vt::new(14, 4).unwrap();
        let mut oracle = Vt::new(14, 4).unwrap();
        let mut cache = Vec::new();
        for terminal in [&mut vt, &mut oracle] {
            terminal.write(
                "\x1b[31mab 한e\u{301}👩\u{200d}💻\r\nhistory\r\nthird\r\nfourth\r\nfifth\x1b[2;1H"
                    .as_bytes(),
            );
        }
        let mut apply = |vt: &mut Vt, oracle: &mut Vt, verify: &dyn Fn(&RenderSnapshot)| {
            let full = render(oracle, true);
            assert_eq!(
                vt.update_render(false, |delta| {
                    verify(&delta);
                    check_cache(delta, full, &mut cache);
                    true
                }),
                Some(true)
            );
        };
        apply(&mut vt, &mut oracle, &|delta| {
            assert_eq!(delta.dirty, RenderDirty::Full)
        });
        apply(&mut vt, &mut oracle, &|delta| {
            assert_eq!(delta.dirty, RenderDirty::Clean);
            assert!(delta.rows.is_empty());
        });

        for terminal in [&mut vt, &mut oracle] {
            terminal.write(b"\x1b[2;1H\x1b[44mXY");
        }
        apply(&mut vt, &mut oracle, &|delta| {
            assert_eq!(delta.dirty, RenderDirty::Partial);
            assert_eq!(
                delta.rows.iter().map(|row| row.row).collect::<Vec<_>>(),
                [1]
            );
        });

        for terminal in [&mut vt, &mut oracle] {
            assert!(terminal.select_start(1, 2, SelectionKind::Cell));
            assert!(terminal.select_update(3, 2));
        }
        apply(&mut vt, &mut oracle, &|delta| {
            assert_eq!(
                delta
                    .rows
                    .iter()
                    .find(|row| row.row == 2)
                    .unwrap()
                    .selection,
                Some(1..=3)
            );
        });
        for terminal in [&mut vt, &mut oracle] {
            terminal.clear_selection();
        }
        apply(&mut vt, &mut oracle, &|_| {});

        for (label, operation) in [
            ("scroll top", Vt::scroll_to_top as fn(&mut Vt)),
            ("scroll bottom", Vt::scroll_to_bottom),
            ("palette", |t| t.write(b"\x1b]4;1;rgb:10/20/30\x07")),
            ("default colors", |t| {
                t.write(b"\x1b]10;#aabbcc\x07\x1b]11;#112233\x07")
            }),
            ("resize", |t| t.resize(8, 3)),
            ("enter alt screen", |t| t.write(b"\x1b[?1049hALT")),
            ("leave alt screen", |t| t.write(b"\x1b[?1049l")),
        ] {
            operation(&mut vt);
            operation(&mut oracle);
            apply(&mut vt, &mut oracle, &|next| {
                assert_eq!(next.dirty, RenderDirty::Full, "{label}")
            });
        }

        for terminal in [&mut vt, &mut oracle] {
            terminal.write(b"\x1b[?25l\x1b[2;3H");
        }
        apply(&mut vt, &mut oracle, &|next| {
            assert_eq!(next.cursor, None);
            assert_eq!(next.input_position, Some((2, 1)));
        });
        let legacy = vt.frame().0.into_iter().map(style_runs).collect::<Vec<_>>();
        assert!(cache.iter().map(|row| &row.runs).eq(legacy.iter()));
        let next = render(&mut vt, false);
        assert_eq!(next.dirty, RenderDirty::Full);
        assert_eq!(next.cursor, None);
        assert_eq!(next.input_position, Some((2, 1)));
        assert_eq!(vt.input_position(), Some((2, 1)));
    }

    #[test]
    fn rejected_cache_update_does_not_clean_and_recovers() {
        let mut vt = Vt::new(8, 2).unwrap();
        assert_eq!(render(&mut vt, false).dirty, RenderDirty::Full);
        vt.write(b"x");
        assert_eq!(
            vt.update_render(false, |delta| {
                assert_eq!(delta.dirty, RenderDirty::Partial);
                false
            }),
            Some(false)
        );
        let mut raw = -1i32;
        assert_eq!(
            unsafe {
                ffi::ghostty_render_state_get(vt.render.0, ffi::RENDER_DATA_DIRTY, out(&mut raw))
            },
            ffi::SUCCESS
        );
        assert_eq!(raw, ffi::RENDER_DIRTY_PARTIAL);
        vt.write(b"\r\ny");
        let retry = render(&mut vt, false);
        assert_eq!(retry.dirty, RenderDirty::Full);
        assert_eq!(retry.rows[0].runs[0].0.text, "x       ");
        assert_eq!(retry.rows[1].runs[0].0.text, "y       ");
        let idle = render(&mut vt, false);
        assert_eq!(idle.dirty, RenderDirty::Clean);
        assert!(idle.rows.is_empty());
        assert_eq!(render(&mut vt, true).dirty, RenderDirty::Full);
        let mut replacement = Vt::new(8, 2).unwrap();
        assert_eq!(render(&mut replacement, false).dirty, RenderDirty::Full);
    }

    #[test]
    fn terminal_selection_tracks_scrollback_and_formats_wrapped_unicode() {
        let mut vt = Vt::new(8, 3).unwrap();
        vt.write("한e\u{301}👩\u{200d}💻XYZtail  \r\nhard  \r\nlast".as_bytes());
        let expected = "한e\u{301}👩\u{200d}💻XYZtail\nhard";
        assert_eq!(
            vt.scrollbar(),
            Scrollbar {
                total: 4,
                offset: 1,
                len: 3
            }
        );
        assert!(!vt.has_selection());
        assert_eq!(vt.selected_text(), None);
        assert!(!vt.select_start(8, 0, SelectionKind::Cell));
        vt.scroll_to_top();
        assert_eq!(vt.scrollbar().offset, 0);
        assert!(vt.select_start(0, 0, SelectionKind::Cell));
        assert!(!vt.has_selection());
        assert!(vt.select_update(7, 2));
        assert_eq!(vt.selected_text().as_deref(), Some(expected));
        assert!(vt.select_start(7, 2, SelectionKind::Cell));
        assert!(vt.select_update(0, 0));
        assert_eq!(vt.selected_text().as_deref(), Some(expected));
        assert!(vt.select_start(0, 0, SelectionKind::Cell));
        vt.scroll_to_bottom();
        assert!(vt.select_update(7, 1));
        assert!(vt.has_selection());
        assert_eq!(vt.selected_text().as_deref(), Some(expected));
        vt.clear_selection();
        assert_eq!(vt.selected_text(), None);
        assert!(!vt.select_update(0, 0));
        assert!(vt.select_start(6, 1, SelectionKind::Cell));
        assert!(vt.select_update(7, 1));
        assert_eq!(vt.selected_text(), None);

        let mut viewer = Vt::new(8, 3).unwrap();
        viewer.write(&vt.paint(1));
        assert_eq!(viewer.scrollbar(), vt.scrollbar());
        viewer.scroll_to_top();
        assert!(viewer.select_start(7, 2, SelectionKind::Cell));
        assert!(viewer.select_update(0, 0));
        assert_eq!(viewer.selected_text().as_deref(), Some(expected));
        // Neither copy selection nor viewport position is transmitted by paint.
        let mut active_only = Vt::new(8, 3).unwrap();
        active_only.write(&vt.paint(0));
        assert_eq!(active_only.scrollback_rows(), 0);
        assert!(!active_only.has_selection());
    }

    #[test]
    fn word_and_line_selection_drag_and_paint_restore() {
        let mut server = Vt::new(16, 2).unwrap();
        server.write(b"first history\r\none two three  \r\nfour five\x1b[?1002h\x1b[?1006h\x1b[?1007l\x1b[?2004h");
        let mut viewer = Vt::new(16, 2).unwrap();
        viewer.write(&server.paint(10));
        for vt in [&mut server, &mut viewer] {
            assert!(vt.mouse_tracking() && vt.mouse_modes().button && vt.mouse_modes().sgr);
            assert!(!vt.alt_scroll());
            assert!(vt.bracketed_paste());
            vt.scroll_to_top();
            assert!(vt.select_start(7, 0, SelectionKind::Word));
            assert_eq!(vt.selected_text().as_deref(), Some("history"));
            vt.scroll_to_bottom();
            assert!(vt.select_update(5, 0));
            assert_eq!(vt.selected_text().as_deref(), Some("history\none two"));
            assert!(vt.select_start(5, 1, SelectionKind::Line));
            assert_eq!(vt.selected_text().as_deref(), Some("four five"));
            assert!(vt.select_update(5, 0));
            assert_eq!(
                vt.selected_text().as_deref(),
                Some("one two three\nfour five")
            );
        }
    }

    #[test]
    fn mouse_reporting_matches_tracking_protocol_and_alt_scroll_mode() {
        let mut vt = Vt::new(10, 4).unwrap();
        let mut event = MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            any_button_pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            position: (17.0, 12.0),
            cell_size: (8, 10),
        };
        vt.write(b"\x1b[?1006h");
        assert!(vt.mouse_modes().sgr);
        assert!(!vt.mouse_tracking());
        assert_eq!(vt.encode_mouse(event), None);
        vt.write(b"\x1b[?1000h");
        assert!(vt.mouse_tracking() && vt.mouse_modes().normal);
        assert_eq!(
            vt.encode_mouse(event).as_deref(),
            Some(&b"\x1b[<0;3;2M"[..])
        );
        event.action = MouseAction::Motion;
        assert_eq!(vt.encode_mouse(event), None);
        event.action = MouseAction::Release;
        assert_eq!(
            vt.encode_mouse(event).as_deref(),
            Some(&b"\x1b[<0;3;2m"[..])
        );
        vt.write(b"\x1b[?1000l\x1b[?1002h");
        assert!(vt.mouse_modes().button);
        event.action = MouseAction::Motion;
        assert_eq!(
            vt.encode_mouse(event).as_deref(),
            Some(&b"\x1b[<32;3;2M"[..])
        );
        event.any_button_pressed = false;
        event.button = None;
        assert_eq!(vt.encode_mouse(event), None);
        vt.write(b"\x1b[?1002l\x1b[?1003h");
        assert!(vt.mouse_modes().any);
        assert_eq!(
            vt.encode_mouse(event).as_deref(),
            Some(&b"\x1b[<35;3;2M"[..])
        );
        event.action = MouseAction::Press;
        event.button = Some(MouseButton::WheelUp);
        event.ctrl = true;
        assert_eq!(
            vt.encode_mouse(event).as_deref(),
            Some(&b"\x1b[<80;3;2M"[..])
        );
        vt.write(b"\x1b[?1006l");
        assert_eq!(vt.encode_mouse(event).as_deref(), Some(&b"\x1b[Mp#\""[..]));
        vt.write(b"\x1b[?1003l\x1b[?1049h\x1b[?1007l");
        assert!(!vt.mouse_tracking() && vt.alt_screen() && !vt.alt_scroll());
        vt.write(b"\x1b[?1007h");
        assert!(vt.alt_scroll());
        assert_eq!(vt.encode_mouse(event), None);
    }

    #[test]
    fn paste_framing_size_safety_and_query_isolation() {
        let mut vt = Vt::new(10, 2).unwrap();
        vt.write(b"\x1b[6n\x1b]52;c;?\x07\x1b]52;c;ZGF0YQ==\x07");
        assert!(!vt.bracketed_paste());
        assert_eq!(
            vt.encode_paste("a\r\nb\nc\rd\x03\x1b]52;c;evil\x07\u{9d}")
                .unwrap(),
            b"a\rb\rc\rd  ]52;c;evil  "
        );
        assert_eq!(vt.encode_paste("x\x1b[201~y").unwrap(), b"x [201~y");
        assert_eq!(vt.key("c", false, true, false).unwrap(), b"\x03");
        vt.write(b"\x1b[?2004h");
        assert!(vt.bracketed_paste());
        assert_eq!(vt.encode_paste(""), Ok(Vec::new()));
        assert_eq!(
            vt.encode_paste("한\r\nx\n\x03").unwrap(),
            "\x1b[200~한\r\nx\n \x1b[201~".as_bytes()
        );
        for payload in ["before\x1b[201~after", "before\u{9b}201~after"] {
            assert_eq!(vt.encode_paste(payload), Err(PasteError::Unsafe));
        }
        assert_eq!(
            vt.encode_paste(&"x".repeat(MAX_PASTE_BYTES)).unwrap().len(),
            MAX_PASTE_BYTES + 12
        );
        assert_eq!(
            vt.encode_paste(&"é".repeat(MAX_PASTE_BYTES / 2 + 1)),
            Err(PasteError::TooLarge)
        );
        assert_eq!(vt.replies(), b"\x1b[1;1R");
        vt.write(b"\x1b[?2004l");
        assert_eq!(
            vt.encode_paste(&"x".repeat(MAX_PASTE_BYTES)).unwrap().len(),
            MAX_PASTE_BYTES
        );
        assert_eq!(
            vt.encode_paste(&"x".repeat(MAX_PASTE_BYTES + 1)),
            Err(PasteError::TooLarge)
        );
    }

    #[test]
    fn runs_keep_color_boundaries_and_wide_glyphs() {
        let mut vt = Vt::new(9, 2).unwrap();
        vt.write("ab\x1b[41m c한\x1b[0me\u{301}z".as_bytes());
        let runs = style_runs(vt.frame().0.remove(0));
        let geometry: Vec<_> = runs.iter().map(|(c, n)| (c.text.as_str(), *n)).collect();
        assert_eq!(
            geometry,
            [("ab", 2), (" c", 2), ("한", 2), ("e\u{301}", 1), ("z ", 2)]
        );
        assert!(runs[1].0.bg.is_some());
        assert_eq!(runs[1].0.bg, runs[2].0.bg);
        assert_eq!(runs[3].0.bg, None);
    }

    #[test]
    fn uniform_rows_batch_and_alternating_colors_stay_separate() {
        let mut vt = Vt::new(160, 2).unwrap();
        vt.write(b"\x1b[32m");
        vt.write(&[b'x'; 160]);
        let row = vt.frame().0.remove(0);
        assert_eq!(row.len(), 160);
        assert_eq!(style_runs(row).len(), 1);

        let mut vt = Vt::new(160, 2).unwrap();
        vt.write(b"\x1b[31mx\x1b[32mx".repeat(80).as_slice());
        assert_eq!(style_runs(vt.frame().0.remove(0)).len(), 160);
    }

    #[test]
    fn directory_name_tracks_shell_reports() {
        let mut vt = Vt::new(8, 2).unwrap();
        assert_eq!(vt.directory_name(), None);
        vt.write(b"\x1b]7;file://localhost/tmp/my%20project\x07");
        assert_eq!(vt.directory_name().as_deref(), Some("my project"));
        vt.write(b"\x1b]7;file://localhost/tmp/other\x07");
        assert_eq!(vt.directory_name().as_deref(), Some("other"));
    }

    #[test]
    fn viewport_colors() {
        let mut vt = Vt::new(8, 2).unwrap();
        vt.write(b"\x1b[38;2;1;2;3;48;2;4;5;6mX");
        let screen = vt.frame().0;
        assert_eq!(screen[0][0].fg, Some(0x010203));
        assert_eq!(screen[0][0].bg, Some(0x040506));
    }

    #[test]
    fn viewport_keys_and_replies() {
        let mut vt = Vt::new(10, 2).unwrap();
        let mut whole = Vt::new(10, 2).unwrap();
        let input = "\x1b[31m한e\u{301}\x1b[0m";
        for byte in input.bytes() {
            vt.write(&[byte]);
        }
        whole.write(input.as_bytes());
        assert_eq!(vt.lines(), whole.lines());
        assert_eq!(vt.lines(), ["한e\u{301}", ""]);
        assert_eq!(vt.frame().1, Some((3, 0)));
        vt.write(b"\x1b[6n\x1b]52;c;?\x07\x1b]52;c;ZGF0YQ==\x07");
        assert_eq!(vt.replies(), b"\x1b[1;4R");
        assert!(vt.replies().is_empty());

        assert_eq!(vt.key("enter", false, false, false).unwrap(), b"\r");
        assert_eq!(
            vt.key("enter", true, false, false).unwrap(),
            b"\x1b[27;2;13~"
        );
        assert_eq!(vt.key("c", false, true, false).unwrap(), b"\x03");
        vt.write(b"\x1b[?1h");
        assert_eq!(vt.key("up", false, false, false).unwrap(), b"\x1bOA");
        vt.write(b"\x1b[>1u");
        assert_eq!(vt.key("enter", false, false, false).unwrap(), b"\r");
        assert_eq!(vt.key("enter", true, false, false).unwrap(), b"\x1b[13;2u");

        vt.write(b"\r\nsecond\r\nthird");
        assert_eq!(vt.lines(), ["second", "third"]);
        vt.write(b"\x1b[?25l");
        assert_eq!(vt.frame().1, None);
        assert_eq!(vt.input_position(), Some((5, 1)));
        vt.scroll(-1);
        assert_eq!(vt.lines(), ["한e\u{301}", "second"]);
        assert_eq!(vt.input_position(), None);
        assert_eq!(vt.frame().1, None);
        vt.scroll(1);
        vt.resize(10, 3);
        assert_eq!(vt.lines().len(), 3);
    }

    #[test]
    fn paint_reproduces_screen_scrollback_colors_alt_screen_and_continuation() {
        let mut server = Vt::new(20, 3).unwrap();
        server.write(b"hist1\r\nhist2\r\n\x1b[38;2;10;20;30mactive\x1b[0m\r\n\x1b]2;mytitle\x07");
        // Split a UTF-8 Korean glyph right at the snapshot boundary!
        let han = "한".as_bytes();
        server.write(&han[..1]);

        let paint = server.paint(10);
        let mut viewer = Vt::new(20, 3).unwrap();
        viewer.write(&paint);
        // Feed remaining bytes live after attach:
        server.write(&han[1..]);
        viewer.write(&han[1..]);

        assert_eq!(viewer.lines(), server.lines());
        assert_eq!(viewer.title(), "mytitle");
        assert_eq!(viewer.frame().0[1][0].fg, Some(0x0a141e));
        assert!(viewer.plain_text().contains("hist1"));
        assert!(viewer.plain_text().contains("한"));

        // Alt screen: paint restores primary behind alt screen too.
        server.write(b"\x1b[?1049hALT_CONTENT");
        let alt_paint = server.paint(10);
        let mut alt_viewer = Vt::new(20, 3).unwrap();
        alt_viewer.write(&alt_paint);
        assert!(alt_viewer.alt_screen());
        assert!(alt_viewer.plain_text().contains("ALT_CONTENT"));
        alt_viewer.write(b"\x1b[?1049l");
        assert!(!alt_viewer.alt_screen());
        assert!(alt_viewer.plain_text().contains("active"));
    }

    #[test]
    fn snapshot_and_write_until_ground_work() {
        let mut vt = Vt::new(16, 2).unwrap();
        vt.write(b"hello\r\nworld");
        let snap = vt.snapshot().expect("snapshot encode");
        let mut restored = Vt::from_snapshot(&snap).expect("snapshot decode");
        assert_eq!(restored.lines(), vt.lines());

        let mut g = Vt::new(16, 2).unwrap();
        g.write(b"\x1b]2;half");
        assert_eq!(g.write_until_ground(b"still-in-osc"), None);
        let consumed = g
            .write_until_ground(b"done\x07after")
            .expect("ground reached");
        assert_eq!(consumed, 5);
        assert_eq!(g.title(), "halfstill-in-oscdone");
    }
}
