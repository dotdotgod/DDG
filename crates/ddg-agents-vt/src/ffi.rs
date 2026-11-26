//! Ghostty VT 1.0.1 C ABI declarations and constants.

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type FreeFn = unsafe extern "C" fn(Handle);
pub type NewFn = unsafe extern "C" fn(*const c_void, *mut Handle) -> i32;

pub const SUCCESS: i32 = 0;
pub const OUT_OF_SPACE: i32 = -3;

// GhosttyTerminalOption
pub const TERMINAL_OPT_USERDATA: i32 = 1;
pub const TERMINAL_OPT_WRITE_PTY: i32 = 2;
pub const TERMINAL_OPT_SCROLLBACK_MAX_BYTES: i32 = 28;
pub const TERMINAL_OPT_CONTINUATION_MAX_BYTES: i32 = 32;

// GhosttyTerminalData
pub const TERMINAL_DATA_COLS: i32 = 1;
pub const TERMINAL_DATA_ROWS: i32 = 2;
pub const TERMINAL_DATA_ACTIVE_SCREEN: i32 = 6;
pub const TERMINAL_DATA_TITLE: i32 = 12;
pub const TERMINAL_DATA_PWD: i32 = 13;
pub const TERMINAL_DATA_SCROLLBACK_ROWS: i32 = 15;

// GhosttyTerminalScreen
pub const TERMINAL_SCREEN_ALTERNATE: i32 = 1;

// GhosttyTerminalBoundary
pub const TERMINAL_BOUNDARY_GROUND: u32 = 1 << 0;

// GhosttyTerminalScrollViewportTag
pub const SCROLL_VIEWPORT_DELTA: i32 = 2;

// GhosttyPointTag
pub const POINT_TAG_ACTIVE: i32 = 0;
pub const POINT_TAG_HISTORY: i32 = 3;

// GhosttyFormatterOption & Format & Extras
pub const FORMATTER_OPT_EMIT: i32 = 1;
pub const FORMATTER_OPT_UNWRAP: i32 = 2;
pub const FORMATTER_OPT_TRIM: i32 = 3;
pub const FORMATTER_OPT_SELECTION: i32 = 4;
pub const FORMATTER_OPT_EXTRAS: i32 = 5;
pub const FORMATTER_FORMAT_PLAIN: i32 = 0;
pub const FORMATTER_FORMAT_VT: i32 = 1;
pub const FORMATTER_EXTRA_PALETTE: u32 = 1 << 0;
pub const FORMATTER_EXTRAS_NONE: u32 = 0;
pub const FORMATTER_EXTRAS_ALL: u32 = 0xFFF;

// GhosttyRenderStateData / RowData / RowCellsData / CellData
pub const RENDER_DATA_COLS: i32 = 1;
pub const RENDER_DATA_ROWS: i32 = 2;
pub const RENDER_DATA_ROW_ITERATOR: i32 = 4;
pub const RENDER_DATA_CURSOR_VISIBLE: i32 = 11;
pub const RENDER_DATA_CURSOR_IN_VIEWPORT: i32 = 14;
pub const RENDER_DATA_CURSOR_VIEWPORT_X: i32 = 15;
pub const RENDER_DATA_CURSOR_VIEWPORT_Y: i32 = 16;
pub const RENDER_ROW_DATA_CELLS: i32 = 3;
pub const RENDER_CELL_DATA_RAW: i32 = 1;
pub const RENDER_CELL_DATA_GRAPHEMES: i32 = 4;
pub const RENDER_CELL_DATA_BG_COLOR: i32 = 5;
pub const RENDER_CELL_DATA_FG_COLOR: i32 = 6;
pub const CELL_DATA_WIDE: i32 = 3;

#[repr(C)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct Codepoints {
    pub ptr: *mut u32,
    pub cap: usize,
    pub len: usize,
}

#[repr(C)]
pub union ScrollValue {
    pub delta: isize,
    pub padding: [u64; 2],
}

#[repr(C)]
pub struct Scroll {
    pub tag: i32,
    pub value: ScrollValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PointCoordinate {
    pub x: u16,
    pub y: u32,
}

#[repr(C)]
pub union PointValue {
    pub coordinate: PointCoordinate,
    pub padding: [u64; 2],
}

#[repr(C)]
pub struct Point {
    pub tag: i32,
    pub value: PointValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GridRef {
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

#[repr(C)]
pub struct Selection {
    pub start: GridRef,
    pub end: GridRef,
    pub rectangle: bool,
}

#[repr(C)]
pub struct WriteStop {
    pub size: usize,
    pub consumed: usize,
    pub reached: u32,
    pub kind: i32,
    pub introducer: i32,
    pub end: i32,
    pub byte: u8,
}

unsafe extern "C" {
    pub fn ghostty_free(alloc: *const c_void, ptr: *mut u8, len: usize);

    pub fn ghostty_terminal_new(a: *const c_void, out: *mut Handle, cols: u16, rows: u16) -> i32;
    pub fn ghostty_terminal_free(terminal: Handle);
    pub fn ghostty_terminal_get(terminal: Handle, data: i32, out: *mut c_void) -> i32;
    pub fn ghostty_terminal_set(terminal: Handle, option: i32, value: *const c_void) -> i32;
    pub fn ghostty_terminal_vt_write(terminal: Handle, bytes: *const u8, len: usize);
    pub fn ghostty_terminal_vt_write_until(
        terminal: Handle,
        data: *const u8,
        len: usize,
        boundaries: u32,
        out: *mut WriteStop,
    ) -> i32;
    pub fn ghostty_terminal_continuation_alloc(
        terminal: Handle,
        alloc: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    pub fn ghostty_terminal_resize(
        terminal: Handle,
        cols: u16,
        rows: u16,
        px_w: u32,
        px_h: u32,
    ) -> i32;
    pub fn ghostty_terminal_scroll_viewport(terminal: Handle, scroll: Scroll) -> i32;
    pub fn ghostty_terminal_grid_ref(terminal: Handle, point: Point, out_ref: *mut GridRef) -> i32;

    pub fn ghostty_formatter_terminal_new(
        alloc: *const c_void,
        out: *mut Handle,
        terminal: Handle,
    ) -> i32;
    pub fn ghostty_formatter_set(formatter: Handle, option: i32, value: *const c_void) -> i32;
    pub fn ghostty_formatter_format_alloc(
        formatter: Handle,
        alloc: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    pub fn ghostty_formatter_free(formatter: Handle);

    pub fn ghostty_snapshot_encode_alloc(
        terminal: Handle,
        alloc: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    pub fn ghostty_snapshot_decoder_new_buf(
        alloc: *const c_void,
        out: *mut Handle,
        ptr: *const u8,
        len: usize,
    ) -> i32;
    pub fn ghostty_snapshot_decoder_decode(decoder: Handle, out_terminal: *mut Handle) -> i32;
    pub fn ghostty_snapshot_decoder_free(decoder: Handle);

    pub fn ghostty_render_state_new(a: *const c_void, out: *mut Handle) -> i32;
    pub fn ghostty_render_state_free(state: Handle);
    pub fn ghostty_render_state_update(state: Handle, terminal: Handle) -> i32;
    pub fn ghostty_render_state_get(state: Handle, data: i32, out: *mut c_void) -> i32;
    pub fn ghostty_render_state_row_iterator_new(a: *const c_void, out: *mut Handle) -> i32;
    pub fn ghostty_render_state_row_iterator_free(iter: Handle);
    pub fn ghostty_render_state_row_iterator_next(iter: Handle) -> bool;
    pub fn ghostty_render_state_row_get(iter: Handle, data: i32, out: *mut c_void) -> i32;
    pub fn ghostty_render_state_row_cells_new(a: *const c_void, out: *mut Handle) -> i32;
    pub fn ghostty_render_state_row_cells_free(cells: Handle);
    pub fn ghostty_render_state_row_cells_next(cells: Handle) -> bool;
    pub fn ghostty_render_state_row_cells_get(cells: Handle, data: i32, out: *mut c_void) -> i32;
    pub fn ghostty_cell_get(cell: u64, data: i32, out: *mut c_void) -> i32;

    pub fn ghostty_key_encoder_new(a: *const c_void, out: *mut Handle) -> i32;
    pub fn ghostty_key_encoder_free(encoder: Handle);
    pub fn ghostty_key_encoder_set_from_terminal(encoder: Handle, terminal: Handle) -> i32;
    pub fn ghostty_key_encoder_encode(
        encoder: Handle,
        event: Handle,
        buf: *mut u8,
        len: usize,
        out: *mut usize,
    ) -> i32;
    pub fn ghostty_key_event_new(a: *const c_void, out: *mut Handle) -> i32;
    pub fn ghostty_key_event_free(event: Handle);
    pub fn ghostty_key_event_set_action(event: Handle, action: i32) -> i32;
    pub fn ghostty_key_event_set_key(event: Handle, key: i32) -> i32;
    pub fn ghostty_key_event_set_mods(event: Handle, mods: u16) -> i32;
    pub fn ghostty_key_event_set_utf8(event: Handle, text: GhosttyString) -> i32;
    pub fn ghostty_key_event_set_unshifted_codepoint(event: Handle, codepoint: u32) -> i32;
}
