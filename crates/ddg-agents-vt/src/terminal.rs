//! Native terminal state, rendering snapshot, VT re-serialization (`paint`),
//! binary snapshot encode/decode, and keyboard encoding via Ghostty VT 1.0.1.

use crate::ffi::{
    self, Codepoints, FreeFn, GhosttyString, GridRef, Handle, NewFn, Point, PointCoordinate,
    PointValue, Scroll, ScrollValue, Selection, WriteStop,
};
use std::{ffi::c_void, ptr, slice};

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

pub struct Vt {
    cells: Owned,
    row_iter: Owned,
    render: Owned,
    encoder: Owned,
    terminal: Owned,
    pending: Vec<u8>,
}

unsafe extern "C" fn collect_reply(_: Handle, data: Handle, bytes: *const u8, len: usize) {
    if !data.is_null() && !bytes.is_null() && len != 0 && len <= isize::MAX as usize {
        // Ghostty borrows the bytes during the callback. Userdata is set only
        // for the duration of the synchronous call that owns the Vec borrow.
        unsafe { &mut *data.cast::<Vec<u8>>() }
            .extend_from_slice(unsafe { slice::from_raw_parts(bytes, len) });
    }
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
            terminal,
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

    fn select(&self, tag: i32, (x0, y0): (u16, u32), (x1, y1): (u16, u32)) -> Option<Selection> {
        Some(Selection {
            start: self.grid_ref(tag, x0, y0)?,
            end: self.grid_ref(tag, x1, y1)?,
            rectangle: false,
        })
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
            if let Some(sel) = selection {
                ffi::ghostty_formatter_set(
                    formatter.0,
                    ffi::FORMATTER_OPT_SELECTION,
                    (sel as *const Selection).cast(),
                );
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
        if history_lines > 0 && available > 0 && !self.alt_screen() {
            let from = available.saturating_sub(history_lines) as u32;
            let to = (available - 1) as u32;
            if let Some(sel) = self.select(ffi::POINT_TAG_HISTORY, (0, from), (last.0, to)) {
                out.extend(self.format(
                    ffi::FORMATTER_FORMAT_VT,
                    Some(&sel),
                    ffi::FORMATTER_EXTRAS_NONE,
                    false,
                    false,
                ));
                // Push formatted history rows into the receiver's scrollback.
                out.extend_from_slice(b"\x1b[0m\r\n");
                out.extend(std::iter::repeat_n(b'\n', usize::from(rows)));
                out.extend_from_slice(b"\x1b[H\x1b[2J");
            }
        }

        if self.alt_screen() {
            out.extend_from_slice(b"\x1b[?1049h\x1b[!p");
        }

        if let Some(sel) = self.select(ffi::POINT_TAG_ACTIVE, (0, 0), last) {
            let extras = ffi::FORMATTER_EXTRAS_ALL & !ffi::FORMATTER_EXTRA_PALETTE;
            let screen = self.format(ffi::FORMATTER_FORMAT_VT, Some(&sel), extras, false, false);
            // Synchronized output would freeze a fresh viewer; alt-screen
            // transitions are already emitted explicitly above.
            out.extend(strip_sequences(
                &screen,
                &[b"\x1b[?2026h", b"\x1b[?47h", b"\x1b[?1047h", b"\x1b[?1049h"],
            ));
        }

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
        if unsafe { ffi::ghostty_render_state_update(self.render.0, self.terminal.0) }
            != ffi::SUCCESS
        {
            return (Vec::new(), None);
        }
        (
            self.viewport_cells().unwrap_or_default(),
            self.viewport_cursor(),
        )
    }

    fn viewport_cells(&mut self) -> Option<Vec<Vec<Cell>>> {
        let (state, iter, cells) = (self.render.0, self.row_iter.0, self.cells.0);
        let mut rows = Vec::new();
        let mut buf = vec![0u32; 16];
        unsafe {
            if ffi::ghostty_render_state_get(
                state,
                ffi::RENDER_DATA_ROW_ITERATOR,
                out(&mut self.row_iter.0),
            ) != ffi::SUCCESS
            {
                return None;
            }
            while ffi::ghostty_render_state_row_iterator_next(iter) {
                if ffi::ghostty_render_state_row_get(
                    iter,
                    ffi::RENDER_ROW_DATA_CELLS,
                    out(&mut self.cells.0),
                ) != ffi::SUCCESS
                {
                    return None;
                }
                let mut line = Vec::new();
                while ffi::ghostty_render_state_row_cells_next(cells) {
                    let mut raw = 0u64;
                    let mut wide = 0i32;
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
                    match wide {
                        0 | 1 => (),
                        2 | 3 => continue, // Wide-character spacer cells carry no text.
                        _ => return None,
                    }
                    let mut points = Codepoints {
                        ptr: buf.as_mut_ptr(),
                        cap: buf.len(),
                        len: 0,
                    };
                    let mut result = ffi::ghostty_render_state_row_cells_get(
                        cells,
                        ffi::RENDER_CELL_DATA_GRAPHEMES,
                        out(&mut points),
                    );
                    if result == ffi::OUT_OF_SPACE && points.len > buf.len() {
                        buf.try_reserve(points.len - buf.len()).ok()?;
                        buf.resize(points.len, 0);
                        points = Codepoints {
                            ptr: buf.as_mut_ptr(),
                            cap: buf.len(),
                            len: 0,
                        };
                        result = ffi::ghostty_render_state_row_cells_get(
                            cells,
                            ffi::RENDER_CELL_DATA_GRAPHEMES,
                            out(&mut points),
                        );
                    }
                    if result != ffi::SUCCESS {
                        return None;
                    }
                    let color = |kind| {
                        let mut channels = [0u8; 3];
                        (ffi::ghostty_render_state_row_cells_get(cells, kind, out(&mut channels))
                            == ffi::SUCCESS)
                            .then(|| u32::from_be_bytes([0, channels[0], channels[1], channels[2]]))
                    };
                    let text = if points.len == 0 {
                        " ".to_owned()
                    } else {
                        buf.get(..points.len)?
                            .iter()
                            .map(|&cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
                            .collect()
                    };
                    line.push(Cell {
                        text,
                        width: if wide == 1 { 2 } else { 1 },
                        bg: color(ffi::RENDER_CELL_DATA_BG_COLOR),
                        fg: color(ffi::RENDER_CELL_DATA_FG_COLOR),
                    });
                }
                rows.push(line);
            }
        }
        Some(rows)
    }

    /// Zero-based viewport (column, row), or `None` when hidden or offscreen.
    fn viewport_cursor(&self) -> Option<(u16, u16)> {
        let state = self.render.0;
        let (mut visible, mut present) = (false, false);
        let (mut x, mut y, mut cols, mut rows) = (0u16, 0u16, 0u16, 0u16);
        unsafe {
            if ffi::ghostty_render_state_get(
                state,
                ffi::RENDER_DATA_CURSOR_VISIBLE,
                out(&mut visible),
            ) != ffi::SUCCESS
                || !visible
                || ffi::ghostty_render_state_get(
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

    /// Move the viewport by rows; negative values scroll back in history.
    pub fn scroll(&mut self, delta: isize) {
        let mut value = ScrollValue { padding: [0; 2] };
        value.delta = delta;
        let result = unsafe {
            ffi::ghostty_terminal_scroll_viewport(
                self.terminal.0,
                Scroll {
                    tag: ffi::SCROLL_VIEWPORT_DELTA,
                    value,
                },
            )
        };
        if result != ffi::SUCCESS {
            eprintln!("Ghostty viewport scroll failed ({result})");
        }
    }

    pub fn key(&mut self, key: &str, shift: bool, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
        let (code, unshifted, text) = key_event(key, shift)?;
        let event = Owned::new(
            "key event",
            ffi::ghostty_key_event_new,
            ffi::ghostty_key_event_free,
        )
        .ok()?;
        let mods = u16::from(shift) | (u16::from(ctrl) << 1) | (u16::from(alt) << 2);
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
    use super::{Vt, style_runs};

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
        vt.scroll(-1);
        assert_eq!(vt.lines(), ["한e\u{301}", "second"]);
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
