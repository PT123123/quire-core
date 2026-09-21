// Database (SPEC §三十九) — the thin end of the object model, and the one
// thing this track proves before anything is drawn: a view realizes the rows
// its viewport can show, and the rows it does not show never become objects.
//
// D0 (2026-09-22) ships the projection and nothing else. The tables, the typed
// property model and the views themselves are D1–D5, and their shape is
// written down in ADR-0060…ADR-0065. What is here is the part of the SPEC's
// first red line that can be measured today:
//
//     10 000 行的库不得全量 realize；视图先算可见窗口再取行
//
// It is pure: no SQL, no Slint, no clock. The row count comes from the caller
// (`COUNT(*)`), the rows come from the caller's fetch of exactly the window
// `window()` computed, and `RowWindow::fetch` is the pair a query takes
// (`LIMIT`/`OFFSET`) — so the rows in memory are the rows the query returned by
// construction rather than by discipline. That is the same argument ADR-0028
// makes for a folded subtree one level down, and the same one ADR-0031 makes
// for a grid: the hidden thing has no representation at all.

/// Extra rows kept realized above and below the visible band, so a scroll of
/// one row does not immediately need a fetch. In rows and not in pixels,
/// because what a window costs is the number of row objects it holds; eight is
/// about half a screen at the grid's row height, and D3 re-measures it.
pub const DEFAULT_OVERSCAN: usize = 8;

/// Row height to assume when a row has not been measured yet — Slint reports a
/// zero height for the first layout pass. Dividing by it would make the window
/// infinite, so the projection floors at 1 px: one frame over-realizes rather
/// than dividing by zero, and nothing is ever lost by it.
const MIN_ROW_HEIGHT: f32 = 1.0;

/// The scroll surface of one view, in the units Slint hands the app: a row is
/// as tall as the last row it measured, and the viewport is the height of the
/// list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewGeometry {
    pub row_height: f32,
    pub viewport_height: f32,
    /// Rows kept outside the visible band; see [`DEFAULT_OVERSCAN`].
    pub overscan: usize,
}

impl ViewGeometry {
    /// The ordinary case: the measured row height, the live viewport, and the
    /// standard overscan. A caller with a reason to over-realize says so.
    pub fn new(row_height: f32, viewport_height: f32) -> Self {
        Self {
            row_height,
            viewport_height,
            overscan: DEFAULT_OVERSCAN,
        }
    }

    /// The height one row is laid out at, with the unmeasured case folded in.
    fn step(&self) -> f32 {
        if self.row_height.is_finite() && self.row_height >= MIN_ROW_HEIGHT {
            self.row_height
        } else {
            MIN_ROW_HEIGHT
        }
    }

    /// Rows the viewport itself shows. Never zero: a viewport shorter than one
    /// row still shows one row, and a window of nothing is a view that cannot
    /// be scrolled into anything.
    fn visible_rows(&self) -> usize {
        let height = if self.viewport_height.is_finite() {
            self.viewport_height.max(0.0)
        } else {
            0.0
        };
        ((height / self.step()).ceil() as usize).max(1)
    }
}

/// Which rows of a view exist as objects at all, for one scroll position.
/// `end` is exclusive, so `len()` is the number of realized rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowWindow {
    pub start: usize,
    pub end: usize,
}

impl RowWindow {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, row: usize) -> bool {
        row >= self.start && row < self.end
    }

    /// The fetch plan: the `LIMIT`/`OFFSET` a windowed read runs, and the
    /// number of rows the caller is allowed to hand back.
    pub fn fetch(&self) -> (usize, usize) {
        (self.len(), self.start)
    }
}

/// The rows `total` of a view realize at scroll offset `scroll_y`, in pixels
/// from the top of the list.
pub fn window(total: usize, geometry: ViewGeometry, scroll_y: f32) -> RowWindow {
    if total == 0 {
        return RowWindow { start: 0, end: 0 };
    }
    // The offset is clamped to the content the way Slint clamps it. A table
    // that shrank under an offset still in flight, or one that never could
    // scroll (shorter than its viewport), must not compute a window near its
    // end while the visible band still starts at row 0: the window has to cover
    // what the viewport shows, and this is the only place that knows it.
    let offset = scroll_y.max(0.0).min(max_scroll_y(total, geometry));
    let first = (offset / geometry.step()) as usize;
    let end = first
        .saturating_add(geometry.visible_rows())
        .saturating_add(geometry.overscan)
        .min(total);
    RowWindow {
        start: first.saturating_sub(geometry.overscan),
        end,
    }
}

/// Furthest the viewport can scroll before it shows the table's last row. A
/// window computed from this offset is a whole screenful; a window computed
/// from a larger one is the tail of the table.
pub fn max_scroll_y(total: usize, geometry: ViewGeometry) -> f32 {
    let content = total as f32 * geometry.step();
    (content - geometry.viewport_height.max(0.0)).max(0.0)
}

/// One row as the grid paints it: the identity, the first column, and the cells
/// of the properties this view makes visible — as text, because the *stored*
/// value is typed (ADR-0062) and this is the painted form of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub record: u64,
    pub title: String,
    pub cells: Vec<String>,
}

/// The realized slice of one view: how many rows the table has, which window of
/// them exists, and exactly those rows. `rows.len() <= window.len()` is
/// maintained by the only constructor, so no code path can hold the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealizedRows {
    total: usize,
    window: RowWindow,
    rows: Vec<RowView>,
}

impl RealizedRows {
    /// Scroll to `scroll_y`: compute the window, then ask `fetch` for exactly
    /// those rows. The red line's sentence is this function — the window is
    /// computed *before* any row is asked for, and the fetch is handed nothing
    /// but the window.
    pub fn scroll_to(
        total: usize,
        geometry: ViewGeometry,
        scroll_y: f32,
        fetch: impl FnOnce(RowWindow) -> Vec<RowView>,
    ) -> Self {
        let window = window(total, geometry, scroll_y);
        let mut rows = fetch(window);
        // A repository that hands back more than the window asked for is a bug,
        // and one that hands back fewer is a short read. Neither may become a
        // model whose length disagrees with the window it claims to show.
        rows.truncate(window.len());
        Self {
            total,
            window,
            rows,
        }
    }

    /// Rows the table has, as the caller's `COUNT(*)` reported them.
    pub fn total(&self) -> usize {
        self.total
    }

    pub fn window(&self) -> RowWindow {
        self.window
    }

    pub fn rows(&self) -> &[RowView] {
        &self.rows
    }

    /// Rows that exist as objects — the number the RAM gate is about, and the
    /// only number in this module that is not the caller's.
    pub fn realized(&self) -> usize {
        self.rows.len()
    }

    /// What the fetch that produced these rows ran.
    pub fn fetch(&self) -> (usize, usize) {
        self.window.fetch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid's own numbers: `benchmarks/scripts/bench.ps1` renders at
    /// 1280×800 and the editor column is a 32 px row.
    const GEOMETRY: ViewGeometry = ViewGeometry {
        row_height: 32.0,
        viewport_height: 720.0,
        overscan: DEFAULT_OVERSCAN,
    };

    fn rows_in(window: RowWindow) -> Vec<RowView> {
        (window.start..window.end)
            .map(|i| RowView {
                record: i as u64,
                title: format!("row {i}"),
                cells: vec!["a".into(), "b".into()],
            })
            .collect()
    }

    /// The number D0 owes, asserted rather than printed: 10 000 rows in, 31 row
    /// objects out, and the 31 is the viewport's arithmetic (23 visible + 8
    /// over + 8 under at the top) — not the table's size. The same table at
    /// 1 000 rows realizes the same 31.
    #[test]
    fn a_view_of_ten_thousand_rows_realizes_its_window_and_not_the_table() {
        let realized = RealizedRows::scroll_to(10_000, GEOMETRY, 0.0, rows_in);
        assert_eq!(realized.total(), 10_000);
        assert_eq!(realized.window(), RowWindow { start: 0, end: 31 });
        assert_eq!(realized.realized(), 31);
        // The window is bounded by the viewport, not by the table: a hundredth
        // of the table realizes the same slice.
        assert_eq!(window(100, GEOMETRY, 0.0), RowWindow { start: 0, end: 31 });
        assert_eq!(window(10_000, GEOMETRY, 0.0), window(1_000_000, GEOMETRY, 0.0));
    }

    /// The window is the query: `LIMIT` and `OFFSET` come out of it, and the
    /// rows that exist are the rows the fetch was told to return.
    #[test]
    fn the_window_is_the_limit_and_offset_the_fetch_runs() {
        let asked = std::cell::Cell::new(None);
        let realized = RealizedRows::scroll_to(10_000, GEOMETRY, 4_000.0, |w| {
            asked.set(Some(w));
            rows_in(w)
        });
        assert_eq!(asked.get(), Some(RowWindow { start: 117, end: 156 }));
        assert_eq!(realized.fetch(), (39, 117));
        assert_eq!(realized.realized(), 39);
        assert_eq!(realized.rows()[0].record, 117);
        assert_eq!(realized.rows()[38].record, 155);
        // A short read is not a longer model, and an empty read is not a panic.
        let short = RealizedRows::scroll_to(10_000, GEOMETRY, 0.0, |_| Vec::new());
        assert_eq!(short.fetch(), (31, 0));
        assert_eq!(short.realized(), 0);
    }

    /// Scrolled to its end the table shows its last screenful — which is a
    /// whole window, because the offset is clamped to the content the way
    /// Slint clamps it, and not a window past the end.
    #[test]
    fn the_bottom_of_the_table_is_a_screenful_and_not_the_overshoot() {
        let bottom = max_scroll_y(10_000, GEOMETRY);
        assert_eq!(bottom, 319_280.0);
        assert_eq!(
            window(10_000, GEOMETRY, bottom),
            RowWindow {
                start: 9_969,
                end: 10_000
            }
        );
        // An offset that outlives the table it was measured on still lands
        // inside it, on a whole screenful rather than on the tail.
        assert_eq!(
            window(10_000, GEOMETRY, 10_000_000.0),
            RowWindow {
                start: 9_969,
                end: 10_000
            }
        );
        assert_eq!(window(5, GEOMETRY, 10_000_000.0), RowWindow { start: 0, end: 5 });
    }

    /// A table smaller than its viewport realizes what it has and stops — the
    /// overscan never reaches outside `0..total`, and a table that cannot
    /// scroll ignores an offset instead of dropping the rows at the top.
    #[test]
    fn a_table_smaller_than_its_viewport_realizes_every_row_it_has() {
        assert_eq!(window(12, GEOMETRY, 0.0), RowWindow { start: 0, end: 12 });
        assert_eq!(window(12, GEOMETRY, 4_000.0), RowWindow { start: 0, end: 12 });
        assert_eq!(window(1, GEOMETRY, 0.0), RowWindow { start: 0, end: 1 });
        assert_eq!(max_scroll_y(12, GEOMETRY), 0.0);
    }

    /// An empty table realizes nothing, and says so instead of asking for a row
    /// that does not exist.
    #[test]
    fn an_empty_table_realizes_nothing() {
        let realized = RealizedRows::scroll_to(0, GEOMETRY, 0.0, |w| {
            assert!(w.is_empty());
            Vec::new()
        });
        assert_eq!(realized.window(), RowWindow { start: 0, end: 0 });
        assert_eq!(realized.fetch(), (0, 0));
        assert!(!RowWindow { start: 0, end: 31 }.is_empty());
        assert!(RowWindow { start: 5, end: 5 }.is_empty());
    }

    /// The two degenerate geometries have to land somewhere sane rather than
    /// divide by zero or realize nothing: an unmeasured row (Slint reports 0
    /// height on the first pass) over-realizes one frame, and a viewport
    /// shorter than one row still realizes that row.
    #[test]
    fn a_degenerate_geometry_is_bounded_and_never_panics() {
        let unmeasured = ViewGeometry::new(0.0, 720.0);
        assert_eq!(unmeasured.step(), MIN_ROW_HEIGHT);
        // 720 visible rows plus the overscan, for one frame: that is the
        // fallback's whole cost, and it is still bounded by the table.
        assert_eq!(window(10_000, unmeasured, 0.0).len(), 720 + DEFAULT_OVERSCAN);
        assert_eq!(window(100, unmeasured, 0.0).len(), 100);
        let sliver = ViewGeometry::new(40.0, 10.0);
        assert_eq!(window(10_000, sliver, 80.0), RowWindow { start: 0, end: 11 });
        assert_eq!(
            window(10_000, ViewGeometry::new(f32::NAN, 720.0), 0.0).len(),
            720 + DEFAULT_OVERSCAN
        );
    }
}

/// D0's measurement. Two windows on the same table — one that holds every row
/// and one that holds the window — and the bytes each costs, counted on the
/// measuring thread by a transparent global allocator.
#[cfg(test)]
mod probe {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::time::Instant;

    thread_local! {
        // `const`-initialized so that reading them from inside `alloc` cannot
        // itself allocate: a lazy `thread_local!` would recurse.
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    /// The system allocator, plus a live-byte counter for whichever thread is
    /// measuring. Other test threads run in parallel and are not armed, so the
    /// count is this thread's alone — which is the only way to weigh one
    /// structure inside a test binary that shares an allocator with the harness.
    struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc(layout);
            if !ptr.is_null() {
                count(layout.size() as isize);
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc_zeroed(layout);
            if !ptr.is_null() {
                count(layout.size() as isize);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            count(-(layout.size() as isize));
            System.dealloc(ptr, layout);
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let out = System.realloc(ptr, layout, new_size);
            if !out.is_null() {
                count(new_size as isize - layout.size() as isize);
            }
            out
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    fn count(delta: isize) {
        let _ = ARMED.try_with(|armed| {
            if armed.get() {
                let _ = LIVE.try_with(|live| live.set(live.get() + delta));
            }
        });
    }

    /// Run `f` and report what its result holds on the heap, in bytes.
    fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        LIVE.with(|live| live.set(0));
        ARMED.with(|armed| armed.set(true));
        let out = f();
        let bytes = LIVE.with(|live| live.get()).max(0) as usize;
        ARMED.with(|armed| armed.set(false));
        (out, bytes)
    }

    fn row(i: usize) -> RowView {
        RowView {
            record: i as u64,
            title: format!("Note {i}"),
            cells: (0..5).map(|c| format!("v{c}-{i}")).collect(),
        }
    }

    /// What a 10 000-row table costs the process it is opened in, four ways.
    /// Printed, not asserted beyond its shape: nothing else in the repo can
    /// weigh a database yet (the app has no database view until D3), so this is
    /// the number D0 owes and the one D8 will compare against.
    #[test]
    #[ignore = "prints a measurement; run with --release --ignored --nocapture"]
    fn a_window_costs_its_rows_and_a_table_costs_all_of_them() {
        const TOTAL: usize = 10_000;
        let geometry = ViewGeometry::new(32.0, 720.0);
        let before = counters::process_bytes();

        let started = Instant::now();
        let (all, all_bytes) = measure(|| (0..TOTAL).map(row).collect::<Vec<_>>());
        let all_ms = started.elapsed().as_secs_f64() * 1e3;

        let started = Instant::now();
        let (ids, ids_bytes) = measure(|| (0..TOTAL as u64).collect::<Vec<u64>>());
        let ids_ms = started.elapsed().as_secs_f64() * 1e3;

        let started = Instant::now();
        let (realized, window_bytes) =
            measure(|| RealizedRows::scroll_to(TOTAL, geometry, 0.0, |w| rows_of(w)));
        let window_ms = started.elapsed().as_secs_f64() * 1e3;
        let after = counters::process_bytes();

        let rows = realized.realized();
        assert_eq!(rows, 31, "the window is the viewport's arithmetic");
        assert!(all.len() == TOTAL && ids.len() == TOTAL);
        // The claim in one line: the window's rows cost a fraction of the
        // table's, and the margin is the table's size over the window's.
        assert!(
            window_bytes * 10 < all_bytes,
            "window {window_bytes} B vs table {all_bytes} B"
        );

        let rows_mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        println!(
            "database window probe: {TOTAL} rows, geometry 32 px row / 720 px viewport \
             / {DEFAULT_OVERSCAN} overscan"
        );
        println!(
            "  realized {rows} rows (start..end = {}..{}) — the other {} rows have no object",
            realized.window().start,
            realized.window().end,
            TOTAL - rows
        );
        println!(
            "  heap: window {window_bytes} B, the table's rows {all_bytes} B \
             ({:.1}x), the table's ids only {ids_bytes} B",
            all_bytes as f64 / window_bytes.max(1) as f64
        );
        println!(
            "  build: all rows {all_ms:.3} ms, ids only {ids_ms:.3} ms, the window's fetch \
             {window_ms:.4} ms"
        );
        match (before, after) {
            (Some((ws0, priv0)), Some((ws1, priv1))) => println!(
                "  process: working set {:.1} -> {:.1} MB, private {:.1} -> {:.1} MB \
                 (Δ {:.1} / {:.1} MB)",
                rows_mb(ws0),
                rows_mb(ws1),
                rows_mb(priv0),
                rows_mb(priv1),
                rows_mb(ws1.saturating_sub(ws0)),
                rows_mb(priv1.saturating_sub(priv0)),
            ),
            _ => println!("  process: not readable on this platform"),
        }
        let (ws_mb, priv_mb) = match after {
            Some((ws, priv_bytes)) => (rows_mb(ws), rows_mb(priv_bytes)),
            None => (0.0, 0.0),
        };
        println!(
            "{{\"label\":\"track3-d0-window\",\"date\":\"2026-09-22\",\
             \"harness\":\"cargo test --release --lib -- --ignored --nocapture\",\
             \"total\":{TOTAL},\"row_height\":32.0,\"viewport_height\":720.0,\
             \"overscan\":{DEFAULT_OVERSCAN},\"realized_top\":{rows},\
             \"realized_middle\":{},\"realized_bottom\":{},\"fetch_limit\":{},\
             \"fetch_offset\":{},\"heap_window_bytes\":{window_bytes},\
             \"heap_all_rows_bytes\":{all_bytes},\"heap_ids_only_bytes\":{ids_bytes},\
             \"heap_ratio\":{:.2},\"process_working_set_mb\":{ws_mb:.1},\
             \"process_private_mb\":{priv_mb:.1}}}",
            window(TOTAL, geometry, 4_000.0).len(),
            window(TOTAL, geometry, max_scroll_y(TOTAL, geometry)).len(),
            realized.fetch().0,
            realized.fetch().1,
            all_bytes as f64 / window_bytes.max(1) as f64,
        );
    }

    fn rows_of(w: RowWindow) -> Vec<RowView> {
        (w.start..w.end).map(row).collect()
    }

    /// This process's working set and private bytes, read with the same two
    /// numbers `benchmarks/scripts/bench.ps1` reports for the app window. The
    /// two declarations are hand-written for the reason ADR-0025 gives (one
    /// `extern` block instead of a crate), and they live here rather than in
    /// `platform` because only a headless test asks for this.
    mod counters {
        #[repr(C)]
        #[derive(Default)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged: usize,
            quota_paged: usize,
            quota_peak_non_paged: usize,
            quota_non_paged: usize,
            pagefile: usize,
            peak_pagefile: usize,
        }

        #[cfg(windows)]
        mod win {
            use super::ProcessMemoryCounters;

            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentProcess() -> isize;
                fn K32GetProcessMemoryInfo(
                    process: isize,
                    counters: *mut ProcessMemoryCounters,
                    bytes: u32,
                ) -> i32;
            }

            /// `(working set, private bytes)` in bytes.
            pub fn process_bytes() -> Option<(usize, usize)> {
                let mut counters = ProcessMemoryCounters::default();
                counters.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
                let ok = unsafe {
                    K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb)
                };
                (ok != 0).then_some((counters.working_set, counters.pagefile))
            }
        }

        #[cfg(not(windows))]
        pub fn process_bytes() -> Option<(usize, usize)> {
            None
        }

        #[cfg(windows)]
        pub use win::process_bytes;
    }
}
