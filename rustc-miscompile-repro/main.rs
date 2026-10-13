//! Standalone reproducer for the rustc 1.99.0 codegen miscompile that forces
//! `opt-level = 1` in the release profile (see `Cargo.toml`).
//!
//! This file is deliberately *not* a cargo target: nothing in the crate depends
//! on it, and `cargo build`/`cargo test`/`cargo clippy` never see it. It exists
//! so the workaround can be re-validated in seconds, and so it can be deleted
//! with confidence once a newer toolchain is in use.
//!
//! It is a self-contained copy of the code that miscompiles in `src/edges.rs`:
//! `EdgeRoute::path_chars` (with its `points[index + 1]` lookahead),
//! `step_dir`, `connector_char` and `rank`. There is no `unsafe`, no interior
//! mutability and no uninitialised memory anywhere in it — the emitted code is
//! simply wrong.
//!
//! Run it with a system rustc (no cargo manifest needed):
//!
//! ```console
//! $ rustc -O main.rs -o /tmp/repro && /tmp/repro     # broken: aborts (SIGILL)
//! $ rustc -C opt-level=1 main.rs -o /tmp/repro && /tmp/repro
//! [((0, 0), '─'), ((2, 0), '┐'), ((2, 2), '│')]
//! OK
//! ```
//!
//! Observed on rustc 1.99.0 (and only there so far):
//!
//! - `-C opt-level=0/1/s/z` and a debug build: correct.
//! - `-C opt-level=2/3`: aborts with `index out of bounds: the len is 3 but the
//!   index is 3` inside `path_chars`, i.e. the bounds check for the
//!   `self.points[index + 1]` lookahead fires against a in-range index.
//! - `-C codegen-units=1`, `-C overflow-checks=on` and rewriting the arithmetic
//!   with `wrapping_sub` do not help; `connector_char`, `step_dir` and the loop
//!   each compile correctly on their own at `opt-level = 3`, only the
//!   combination is miscompiled.
//!
//! Because `path_chars` drives the real TUI's box-drawing output, the release
//! binary was not merely failing a test: its rendering could be wrong. The
//! census assertion in `tests/ui_render.rs` now guards the rendered output in
//! every profile.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    North,
    East,
    South,
    West,
}

fn step_dir(from: Point, to: Point) -> Option<Dir> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    if dx == 0 && dy == 0 {
        return None;
    }
    if dx.abs() >= dy.abs() {
        Some(if dx > 0 { Dir::East } else { Dir::West })
    } else {
        Some(if dy > 0 { Dir::South } else { Dir::North })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRoute {
    pub points: Vec<Point>,
}

impl EdgeRoute {
    pub fn path_chars(&self) -> Vec<(Point, char)> {
        let mut out = Vec::with_capacity(self.points.len());
        for index in 0..self.points.len() {
            let point = self.points[index];
            let incoming = if index == 0 {
                None
            } else {
                step_dir(point, self.points[index - 1])
            };
            let outgoing = if index + 1 == self.points.len() {
                None
            } else {
                step_dir(point, self.points[index + 1])
            };
            out.push((point, connector_char(incoming, outgoing)));
        }
        out
    }
}

const fn rank(dir: &Dir) -> u8 {
    match dir {
        Dir::North => 0,
        Dir::East => 1,
        Dir::South => 2,
        Dir::West => 3,
    }
}

const fn straight_char(dir: &Dir) -> char {
    match dir {
        Dir::East | Dir::West => '─',
        Dir::North | Dir::South => '│',
    }
}

pub fn connector_char(incoming: Option<Dir>, outgoing: Option<Dir>) -> char {
    match (incoming, outgoing) {
        (Some(first), Some(second)) => {
            let mut dirs = [first, second];
            dirs.sort_by_key(rank);
            match (dirs[0], dirs[1]) {
                (Dir::East, Dir::West) => '─',
                (Dir::North, Dir::South) => '│',
                (Dir::East, Dir::South) => '┌',
                (Dir::South, Dir::West) => '┐',
                (Dir::North, Dir::East) => '└',
                (Dir::North, Dir::West) => '┘',
                (first, second) if first == second => straight_char(&first),
                _ => '·',
            }
        }
        (Some(dir), None) | (None, Some(dir)) => straight_char(&dir),
        (None, None) => '·',
    }
}

fn main() {
    let route = EdgeRoute {
        points: vec![Point::new(0, 0), Point::new(2, 0), Point::new(2, 2)],
    };
    let chars = route.path_chars();
    println!("{chars:?}");
    assert_eq!(chars[0].1, '─');
    assert_eq!(chars[1].1, '┐');
    assert_eq!(chars[2].1, '│');
    println!("OK");
}
