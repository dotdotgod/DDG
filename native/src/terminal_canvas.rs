use super::{perf, perf_enabled, terminal_input::TerminalInput};
use ddg_agents_vt::{
    Vt,
    terminal::{RenderDirty, RenderSnapshot, RowId, Scrollbar},
};
use gpui::{prelude::*, *};
use std::{cell::Cell, ops::Range, rc::Rc, time::Instant};

const DEFAULT_FG: u32 = 0xf2f2f2;
const DEFAULT_BG: u32 = 0x191919;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FontGrid {
    pub(super) family: &'static str,
    pub(super) width: f32,
    pub(super) height: f32,
    pub(super) font_size: f32,
}

impl Default for FontGrid {
    fn default() -> Self {
        Self {
            family: "Menlo",
            width: 7.,
            height: 14.,
            font_size: super::BASE_TERM_FONT_PX,
        }
    }
}

#[derive(Default)]
pub(super) struct PaneCache {
    // One cache per VT lifetime; the owner resets this on replacement.
    key: Option<(FontGrid, f32)>,
    frame: Frame,
}

#[derive(Clone)]
pub(super) struct Frame {
    grid: FontGrid,
    size: (u16, u16),
    rows: Rc<Vec<Row>>,
    cursor: Option<(u16, u16)>,
    input_position: Option<(u16, u16)>,
    foreground: u32,
    background: u32,
    scrollbar: Scrollbar,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            grid: FontGrid::default(),
            size: (0, 0),
            rows: Rc::default(),
            cursor: None,
            input_position: None,
            foreground: DEFAULT_FG,
            background: DEFAULT_BG,
            scrollbar: Scrollbar::default(),
        }
    }
}

#[derive(Clone)]
struct Row {
    id: RowId,
    selection: Option<Range<usize>>,
    spans: Rc<Vec<Span>>,
}

struct Span {
    columns: Range<usize>,
    text: SharedString,
    fg: Option<u32>,
    bg: Option<u32>,
    line: Option<ShapedLine>,
}

fn shape(
    window: &Window,
    grid: FontGrid,
    text: SharedString,
    color: u32,
    fixed_ascii: bool,
    underline: bool,
) -> ShapedLine {
    let color = rgb(color).into();
    let run = TextRun {
        len: text.len(),
        font: font(grid.family),
        color,
        background_color: None,
        underline: underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: None,
    };
    window.text_system().shape_line(
        text,
        px(grid.font_size),
        &[run],
        fixed_ascii.then_some(px(grid.width)),
    )
}

pub(super) fn prepare(
    cache: &mut PaneCache,
    vt: &mut Vt,
    grid: FontGrid,
    window: &Window,
    pane_id: u64,
) -> Frame {
    if cache.key.is_none() {
        vt.set_default_colors(DEFAULT_FG, DEFAULT_BG);
    }
    let key = (grid, window.scale_factor());
    let force_full = cache.key != Some(key);
    let started = perf_enabled().then(Instant::now);
    let mut next = cache.frame.clone();
    let result = vt.update_render(force_full, |snapshot| {
        let extract_us = started.map(|at| at.elapsed().as_micros());
        let compose_start = extract_us.map(|_| Instant::now());
        let rows = snapshot.rows.len();
        let runs = extract_us.map(|_| snapshot.rows.iter().map(|r| r.runs.len()).sum::<usize>());
        let cols = usize::from(snapshot.size.0);
        let reshapes = apply(&mut next, snapshot, grid, window, force_full);
        if let (Some(extract_us), Some(compose_start), Some(runs)) =
            (extract_us, compose_start, runs)
            && rows > 0
        {
            perf(|p| {
                let pane = p.panes.entry(pane_id).or_default();
                pane.frames += 1;
                pane.rows += rows;
                pane.cells += rows * cols;
                pane.runs += runs;
                pane.reshapes += reshapes;
                pane.extract_us += extract_us;
                pane.compose_us += compose_start.elapsed().as_micros();
            });
        }
        true
    });
    if result == Some(true) {
        next.scrollbar = vt.scrollbar();
        cache.frame = next;
        cache.key = Some(key);
    }
    cache.frame.clone()
}

fn apply(
    frame: &mut Frame,
    snapshot: RenderSnapshot,
    grid: FontGrid,
    window: &Window,
    force_full: bool,
) -> usize {
    let reuse = !force_full
        && frame.size == snapshot.size
        && frame.foreground == snapshot.foreground
        && frame.background == snapshot.background;
    frame.grid = grid;
    frame.size = snapshot.size;
    frame.foreground = snapshot.foreground;
    frame.background = snapshot.background;
    frame.cursor = snapshot.cursor;
    frame.input_position = snapshot.input_position;

    if !reuse && snapshot.dirty == RenderDirty::Full {
        frame.rows = Rc::new(Vec::with_capacity(usize::from(snapshot.size.1)));
    }
    let mut reshapes = 0;
    for update in snapshot.rows {
        let rows = Rc::make_mut(&mut frame.rows);
        let row = usize::from(update.row);
        let selection = update
            .selection
            .map(|range| usize::from(*range.start())..usize::from(*range.end()) + 1);
        // Ghostty can mark selection as a full update; reuse only verified identical spans.
        if reuse
            && let Some(old) = rows.get_mut(row)
            && old.id == update.id
            && old.spans.len() == update.runs.len()
            && old
                .spans
                .iter()
                .zip(&update.runs)
                .all(|(span, (cell, cols))| {
                    span.columns.len() == *cols
                        && span.text.as_ref() == cell.text
                        && span.fg == cell.fg
                        && span.bg == cell.bg
                })
        {
            old.selection = selection;
            continue;
        }
        let mut col = 0;
        let mut shaped = false;
        let spans = update
            .runs
            .into_iter()
            .map(|(cell, cols)| {
                let text = SharedString::from(cell.text);
                let line = (!text.bytes().all(|ch| ch == b' ')).then(|| {
                    shaped = true;
                    // Only VT-coalesced printable ASCII runs may use uniform glyph advances.
                    // Complex clusters get one independently positioned, clipped span.
                    let fixed_ascii =
                        text.len() == cols && text.bytes().all(|ch| matches!(ch, b' '..=b'~'));
                    shape(
                        window,
                        grid,
                        text.clone(),
                        cell.fg.unwrap_or(frame.foreground),
                        fixed_ascii,
                        false,
                    )
                });
                let columns = col..col + cols;
                col += cols;
                Span {
                    columns,
                    text,
                    fg: cell.fg,
                    bg: cell.bg,
                    line,
                }
            })
            .collect();
        reshapes += usize::from(shaped);
        let update = Row {
            id: update.id,
            selection,
            spans: Rc::new(spans),
        };
        if row == rows.len() {
            rows.push(update);
        } else {
            rows[row] = update;
        }
    }
    reshapes
}

type BoundsSlot = Rc<Cell<Option<Bounds<Pixels>>>>;
type CaptureMove = Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>;

pub(super) struct BodyOptions {
    pub(super) pane_id: u64,
    pub(super) tab_id: String,
    pub(super) show_cursor: bool,
    pub(super) ime: Option<(FocusHandle, Entity<TerminalInput>)>,
    pub(super) bounds: BoundsSlot,
    pub(super) ime_bounds: BoundsSlot,
    pub(super) capture_move: Option<CaptureMove>,
}

pub(super) fn body(frame: Frame, options: BodyOptions) -> impl IntoElement {
    let start = perf_enabled().then(Instant::now);
    let BodyOptions {
        pane_id,
        tab_id,
        show_cursor,
        ime,
        bounds,
        ime_bounds,
        capture_move,
    } = options;
    let body = TerminalBody {
        frame,
        pane_id,
        show_cursor,
        ime,
        bounds,
        ime_bounds,
        capture_move,
        interaction: Interactivity::default(),
    }
    .size_full()
    .debug_selector(|| format!("terminal-body-{tab_id}"));
    if let Some(start) = start {
        perf(|p| {
            let pane = p.panes.entry(pane_id).or_default();
            pane.builds += 1;
            pane.compose_us += start.elapsed().as_micros();
        });
    }
    body
}

struct TerminalBody {
    frame: Frame,
    pane_id: u64,
    show_cursor: bool,
    ime: Option<(FocusHandle, Entity<TerminalInput>)>,
    bounds: BoundsSlot,
    ime_bounds: BoundsSlot,
    capture_move: Option<CaptureMove>,
    interaction: Interactivity,
}

impl InteractiveElement for TerminalBody {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interaction
    }
}

impl Styled for TerminalBody {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interaction.base_style
    }
}

impl IntoElement for TerminalBody {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalBody {
    type RequestLayoutState = ();
    type PrepaintState = (Option<Hitbox>, Option<Bounds<Pixels>>);

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let layout =
            self.interaction
                .request_layout(id, inspector, window, cx, |style, window, cx| {
                    window.request_layout(style, [], cx)
                });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.bounds.set(Some(bounds));
        let anchor = self
            .ime
            .as_ref()
            .and_then(|_| self.frame.ime_anchor(bounds));
        self.ime_bounds.set(anchor);
        let hitbox = self.interaction.prepaint(
            id,
            inspector,
            bounds,
            bounds.size,
            window,
            cx,
            |_, _, hitbox, _, _| hitbox,
        );
        (hitbox, anchor)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        (hitbox, anchor): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.interaction.paint(
            id,
            inspector,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |_, window, cx| {
                perf(|p| p.panes.entry(self.pane_id).or_default().paints += 1);
                if let Some(capture_move) = self.capture_move.take() {
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            capture_move(event, window, cx);
                        }
                    });
                }
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    self.frame.paint(bounds, self.show_cursor, window, cx);
                    if let (Some((focus, input)), Some(anchor)) = (&self.ime, *anchor) {
                        window.handle_input(
                            focus,
                            ElementInputHandler::new(anchor, input.clone()),
                            cx,
                        );
                        let marked = input.read(cx).marked.clone();
                        if !marked.is_empty() {
                            self.frame.paint_preedit(anchor, &marked, window, cx);
                        }
                    }
                    self.frame.paint_scrollbar(bounds, window, cx);
                });
            },
        );
    }
}

fn cell_rect(
    bounds: Bounds<Pixels>,
    grid: FontGrid,
    row: usize,
    cols: Range<usize>,
) -> Bounds<Pixels> {
    let x = |col| bounds.origin.x + px(col as f32 * grid.width);
    Bounds::new(
        point(
            x(cols.start),
            bounds.origin.y + px(row as f32 * grid.height),
        ),
        size(x(cols.end) - x(cols.start), px(grid.height)),
    )
}

impl Frame {
    fn ime_anchor(&self, bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        // No viewport cursor in scrollback: retain input at the top-left so typing
        // can return to the live viewport, rather than unregistering the handler.
        let (col, row) = self.input_position.unwrap_or_default();
        let col = usize::from(col);
        let rect = cell_rect(bounds, self.grid, usize::from(row), col..col + 1);
        rect.intersects(&bounds).then(|| {
            Bounds::from_corners(rect.origin, rect.bottom_right().min(&bounds.bottom_right()))
        })
    }

    fn paint(&self, bounds: Bounds<Pixels>, show_cursor: bool, window: &mut Window, cx: &mut App) {
        window.paint_quad(fill(bounds, rgb(self.background)));
        for (index, row) in self.rows.iter().enumerate() {
            let rect = |cols| cell_rect(bounds, self.grid, index, cols);
            if rect(0..usize::from(self.size.0)).top() >= bounds.bottom() {
                break;
            }
            for span in row.spans.iter() {
                if let Some(bg) = span.bg {
                    window.paint_quad(fill(rect(span.columns.clone()), rgb(bg)));
                }
            }
            if let Some(selection) = &row.selection {
                window.paint_quad(fill(rect(selection.clone()), rgb(0x2f4f7f)));
            }
            for span in row.spans.iter() {
                if let Some(line) = &span.line {
                    let clip = rect(span.columns.clone());
                    if clip.intersects(&bounds) {
                        window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                            let _ = line.paint(clip.origin, px(self.grid.height), window, cx);
                        });
                    }
                }
            }
        }
        if show_cursor && let Some((col, row)) = self.cursor {
            let col = usize::from(col);
            let mut rect = cell_rect(bounds, self.grid, usize::from(row), col..col + 1);
            let unit = px(self.grid.font_size / super::BASE_TERM_FONT_PX);
            rect.origin.y = rect.bottom() - unit * 3.;
            rect.size.height = unit * 2.;
            window.paint_quad(fill(rect, rgb(self.foreground)));
        }
    }

    fn paint_preedit(
        &self,
        anchor: Bounds<Pixels>,
        marked: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line = shape(
            window,
            self.grid,
            marked.replace(['\r', '\n'], " ").into(),
            self.foreground,
            false,
            true,
        );
        window.paint_quad(fill(
            Bounds::new(
                anchor.origin,
                size(line.width.max(anchor.size.width), anchor.size.height),
            ),
            rgb(self.background),
        ));
        let _ = line.paint(anchor.origin, px(self.grid.height), window, cx);
    }

    fn paint_scrollbar(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let scrollbar = self.scrollbar;
        let Some(badge) = scroll_badge_bounds(bounds, scrollbar, self.grid) else {
            return;
        };
        let available = bounds.size.height;
        let height = (available * (scrollbar.len as f32 / scrollbar.total as f32))
            .max(px(self.grid.height / 2.))
            .min(available);
        let fraction = scrollbar.offset as f32 / (scrollbar.total - scrollbar.len) as f32;
        let thumb_width = px(self.grid.width * 0.35);
        let thumb = Bounds::new(
            point(
                bounds.right() - thumb_width,
                bounds.top() + (available - height) * fraction,
            ),
            size(thumb_width, height),
        );
        window.paint_quad(fill(thumb, rgb(0x68778a)));
        window.paint_quad(fill(badge, rgb(0x2f4f7f)));
        let text = scroll_badge_text(history_rows(scrollbar));
        let line = shape(window, self.grid, text.into(), 0xf2f2f2, false, false);
        window.with_content_mask(Some(ContentMask { bounds: badge }), |window| {
            let _ = line.paint(
                badge.origin + point(px(self.grid.width * 0.5), px(0.)),
                px(self.grid.height),
                window,
                cx,
            );
        });
    }
}

fn history_rows(scrollbar: Scrollbar) -> u64 {
    scrollbar
        .total
        .saturating_sub(scrollbar.offset.saturating_add(scrollbar.len))
}

fn scroll_badge_text(history: u64) -> String {
    if history == 1 {
        "↑ 1 line".into()
    } else {
        format!("↑ {history} lines")
    }
}

pub(super) fn scroll_badge_bounds(
    bounds: Bounds<Pixels>,
    scrollbar: Scrollbar,
    grid: FontGrid,
) -> Option<Bounds<Pixels>> {
    let history = history_rows(scrollbar);
    if history == 0 || bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return None;
    }
    let right = (bounds.right() - px(grid.width * 0.75)).max(bounds.left());
    let cells = scroll_badge_text(history).chars().count() as f32 + 1.;
    let width = px(grid.width * cells).min(right - bounds.left());
    let height = px(grid.height).min(bounds.size.height);
    (width > px(0.)).then(|| Bounds::new(point(right - width, bounds.top()), size(width, height)))
}

#[cfg(test)]
mod tests {
    use super::{
        BodyOptions, BoundsSlot, FontGrid, Frame, PaneCache, Span, body, cell_rect, prepare,
        scroll_badge_bounds, scroll_badge_text,
    };
    use crate::{PERF, TEST_PERF};
    use ddg_agents_vt::{
        Vt,
        terminal::{Scrollbar, SelectionKind},
    };
    use gpui::{
        Bounds, Context, IntoElement, Pixels, Render, TestAppContext, Window, point, px, size,
    };
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn scrollback_badge_fits_singular_and_plural_labels() {
        let grid = FontGrid::default();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(100.)));
        for (history, label) in [(1, "↑ 1 line"), (12345, "↑ 12345 lines")] {
            let scrollbar = Scrollbar {
                total: history + 10,
                offset: 0,
                len: 10,
            };
            assert_eq!(scroll_badge_text(history), label);
            let badge = scroll_badge_bounds(bounds, scrollbar, grid).unwrap();
            assert_eq!(
                badge.size.width,
                px(grid.width * (label.chars().count() + 1) as f32)
            );
        }
    }

    fn compare(actual: &Frame, full: &Frame, bounds: Bounds<Pixels>) {
        assert_eq!(actual.size, full.size);
        assert_eq!(actual.rows.len(), full.rows.len());
        assert_eq!(actual.cursor, full.cursor);
        assert_eq!(actual.ime_anchor(bounds), full.ime_anchor(bounds));
        assert_eq!(actual.foreground, full.foreground);
        assert_eq!(actual.background, full.background);
        assert_eq!(actual.scrollbar, full.scrollbar);
        for (row, full) in actual.rows.iter().zip(full.rows.iter()) {
            assert_eq!(row.selection, full.selection);
            assert_eq!(row.spans.len(), full.spans.len());
            for (span, full) in row.spans.iter().zip(full.spans.iter()) {
                assert_eq!(span.columns, full.columns);
                assert_eq!(span.fg, full.fg);
                assert_eq!(span.bg, full.bg);
                assert_eq!(span.text, full.text);
                let glyphs = |s: &Span| {
                    s.line.as_ref().map(|line| {
                        line.runs
                            .iter()
                            .flat_map(|run| run.glyphs.iter())
                            .map(|g| (g.id, g.position, g.index, g.is_emoji))
                            .collect::<Vec<_>>()
                    })
                };
                assert_eq!(glyphs(span), glyphs(full));
            }
        }
    }

    #[gpui::test]
    fn dirty_canvas_matches_full_oracle(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|window, _| {
            let grid = FontGrid::default();
            assert_eq!((grid.width, grid.height, grid.font_size), (7., 14., 12.));
            let bounds = Bounds::new(point(px(30.), px(70.)), size(px(84.), px(42.)));
            let last_cell = cell_rect(bounds, grid, 2, 11..12);
            assert_eq!(last_cell.right(), bounds.right());
            assert_eq!(last_cell.bottom(), bounds.bottom());
            let (mut vt, mut oracle) = (Vt::new(12, 3).unwrap(), Vt::new(12, 3).unwrap());
            let mut cache = PaneCache::default();
            TEST_PERF.set(true);
            let initial = "\x1b[?2027h\x1b[31;44mA한e\u{301}👩\u{200d}💻Z\x1b[0m\r\nsecond";
            for bytes in [
                initial,
                "\x1b[2;3H!",
                "\x1b[?25l\x1b[1;4H",
                "\x1b]11;#000000\x07",
                "\x1b]10;#ffffff\x07",
                "\x1b[?1049hALT",
                "\x1b[?1049l",
            ] {
                vt.write(bytes.as_bytes());
                oracle.write(bytes.as_bytes());
                let frame = prepare(&mut cache, &mut vt, grid, window, 1);
                let full = prepare(&mut PaneCache::default(), &mut oracle, grid, window, 2);
                compare(&frame, &full, bounds);
                let sample = PERF.with_borrow_mut(std::mem::take);
                if bytes == initial {
                    let spans = &frame.rows[0].spans;
                    assert_eq!(sample.panes[&1].rows, 3);
                    assert_eq!(sample.panes[&1].reshapes, 2);
                    assert_eq!(frame.foreground, super::DEFAULT_FG);
                    assert_eq!(frame.background, super::DEFAULT_BG);
                    for (text, columns) in [
                        ("A", 0..1),
                        ("한", 1..3),
                        ("e\u{301}", 3..4),
                        ("👩\u{200d}💻", 4..6),
                        ("Z", 6..7),
                    ] {
                        let span = spans
                            .iter()
                            .find(|span| span.text.as_ref() == text)
                            .unwrap();
                        assert_eq!(span.columns, columns);
                        assert!(span.line.is_some());
                        let clip = cell_rect(bounds, grid, 0, span.columns.clone());
                        assert_eq!(
                            clip.left(),
                            bounds.left() + px(grid.width * columns.start as f32)
                        );
                        assert_eq!(clip.size.width, px(grid.width * columns.len() as f32));
                    }
                } else if bytes == "\x1b[2;3H!" {
                    assert_eq!(sample.panes[&1].rows, 1);
                    assert_eq!(sample.panes[&1].reshapes, 1);
                } else if bytes.starts_with("\x1b[?25l") {
                    assert_eq!(frame.cursor, None);
                    assert_eq!(
                        frame.ime_anchor(bounds),
                        Some(cell_rect(bounds, grid, 0, 3..4))
                    );
                } else if bytes.starts_with("\x1b]11") {
                    assert_eq!(frame.foreground, super::DEFAULT_FG);
                    assert_eq!(frame.background, 0x000000);
                } else if bytes.starts_with("\x1b]10") {
                    assert_eq!(frame.foreground, 0xffffff);
                    assert_eq!(frame.background, 0x000000);
                }
                let clean = prepare(&mut cache, &mut vt, grid, window, 1);
                assert!(Rc::ptr_eq(&frame.rows, &clean.rows));
                assert!(PERF.with_borrow(|p| p.panes.is_empty()));
            }

            for terminal in [&mut vt, &mut oracle] {
                assert!(terminal.select_start(1, 0, SelectionKind::Cell));
                assert!(terminal.select_update(5, 0));
            }
            let selected = prepare(&mut cache, &mut vt, grid, window, 1);
            let full = prepare(&mut PaneCache::default(), &mut oracle, grid, window, 2);
            compare(&selected, &full, bounds);
            assert_eq!(selected.rows[0].selection, Some(1..6));
            assert_eq!(PERF.with_borrow(|p| p.panes[&1].reshapes), 0);
            PERF.with_borrow_mut(std::mem::take);

            let zoom = FontGrid {
                font_size: 16.,
                ..grid
            };
            let zoomed = prepare(&mut cache, &mut vt, zoom, window, 1);
            let full = prepare(&mut PaneCache::default(), &mut oracle, zoom, window, 2);
            compare(&zoomed, &full, bounds);
            assert_eq!(PERF.with_borrow(|p| p.panes[&1].rows), 3);
            PERF.with_borrow_mut(std::mem::take);

            for terminal in [&mut vt, &mut oracle] {
                terminal.resize(7, 2);
                terminal.write(b"\r\n\r\n\r\n");
                terminal.scroll_to_top();
            }
            let scrolled = prepare(&mut cache, &mut vt, zoom, window, 1);
            let full = prepare(&mut PaneCache::default(), &mut oracle, zoom, window, 2);
            compare(&scrolled, &full, bounds);
            let badge = scroll_badge_bounds(bounds, scrolled.scrollbar, zoom).unwrap();
            assert_eq!(badge.top(), bounds.top());
            assert!(bounds.contains(&badge.origin));

            let mut wide_wrap = Vt::new(4, 2).unwrap();
            wide_wrap.write("abc한Z".as_bytes());
            let wrapped = prepare(&mut PaneCache::default(), &mut wide_wrap, grid, window, 1);
            assert_eq!(wrapped.rows[0].spans[0].columns, 0..3);
            assert_eq!(wrapped.rows[1].spans[0].text.as_ref(), "한");
            assert_eq!(wrapped.rows[1].spans[0].columns, 0..2);
            assert_eq!(wrapped.rows[1].spans[1].columns.start, 2);
            PERF.with_borrow_mut(std::mem::take);
            TEST_PERF.set(false);
        });
    }

    struct BodyView(BoundsSlot);

    impl Render for BodyView {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let mut vt = Vt::new(30, 12).unwrap();
            vt.write("A한👩\u{200d}💻Z".as_bytes());
            let frame = prepare(
                &mut PaneCache::default(),
                &mut vt,
                FontGrid::default(),
                window,
                3,
            );
            body(
                frame,
                BodyOptions {
                    pane_id: 3,
                    tab_id: "single".into(),
                    show_cursor: true,
                    ime: None,
                    bounds: self.0.clone(),
                    ime_bounds: Rc::default(),
                    capture_move: None,
                },
            )
        }
    }

    #[gpui::test]
    fn one_body_is_built_and_painted_for_many_rows(cx: &mut TestAppContext) {
        TEST_PERF.set(true);
        let bounds = Rc::new(Cell::new(None));
        let (_, cx) = cx.add_window_view(|_, _| BodyView(bounds.clone()));
        assert!(bounds.get().is_some());
        assert_eq!(cx.debug_bounds("terminal-body-single"), bounds.get());
        let sample = PERF.with_borrow_mut(std::mem::take);
        let pane = &sample.panes[&3];
        assert_eq!((pane.rows, pane.builds, pane.paints), (12, 1, 1));
        TEST_PERF.set(false);
    }
}
