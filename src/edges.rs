//! Foreign key edges: orthogonal routing with A*.
//!
//! Every edge is a polyline on the same integer grid the boxes live on, so it can
//! be drawn with box-drawing characters and clipped by the canvas like everything
//! else. Routing boxes are expensive to cross but not forbidden — a database with
//! a self-referencing table would otherwise have unroutable edges — so A* costs 1
//! for an empty cell and [`RouteOptions::occupied_cost`] for a cell inside a box.
//! The Manhattan heuristic stays admissible because no step is cheaper than 1.
//!
//! When A* exhausts its budget the edge falls back to a route along the box-free
//! ring just outside the diagram. A fallback that cannot reach that ring without
//! crossing a box is dropped, so an edge is never drawn as a line reaching
//! nothing.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use ratatui::style::{Color, Style};

/// One hue per connection: each foreign key is painted with its own colour, so
/// the same key stays recognisable across relayouts and zoom changes. Chosen to
/// avoid [`Color::White`] and [`Color::DarkGray`], which the frame already uses.
pub const EDGE_PALETTE: [Color; 8] = [
    Color::Red,
    Color::Yellow,
    Color::Green,
    Color::Cyan,
    Color::Blue,
    Color::Magenta,
    Color::LightRed,
    Color::LightCyan,
];

use crate::canvas::Painter;
use crate::layout::{DiagramLayout, Placement};
use crate::model::Schema;

/// A cell on the integer world grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }

    /// Step the point one cell in `direction`.
    pub const fn step(self, direction: Dir) -> Self {
        let (dx, dy) = direction.delta();
        Point::new(self.x + dx, self.y + dy)
    }

    /// Manhattan distance.
    pub const fn manhattan(self, other: Point) -> u32 {
        (self.x - other.x).unsigned_abs() + (self.y - other.y).unsigned_abs()
    }
}

impl From<Point> for (i32, i32) {
    fn from(point: Point) -> Self {
        (point.x, point.y)
    }
}

/// One of the four grid directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    North,
    East,
    South,
    West,
}

impl Dir {
    /// All four directions, in a fixed order so routing is deterministic.
    pub const ALL: [Dir; 4] = [Dir::North, Dir::East, Dir::South, Dir::West];

    /// Unit step for this direction.
    pub const fn delta(self) -> (i32, i32) {
        match self {
            Dir::North => (0, -1),
            Dir::East => (1, 0),
            Dir::South => (0, 1),
            Dir::West => (-1, 0),
        }
    }

    /// The direction pointing the other way.
    pub const fn opposite(self) -> Dir {
        match self {
            Dir::North => Dir::South,
            Dir::East => Dir::West,
            Dir::South => Dir::North,
            Dir::West => Dir::East,
        }
    }

    /// Which direction a step from `from` to `to` went, if they are adjacent.
    pub fn between(from: Point, to: Point) -> Option<Dir> {
        match (to.x - from.x, to.y - from.y) {
            (0, -1) => Some(Dir::North),
            (1, 0) => Some(Dir::East),
            (0, 1) => Some(Dir::South),
            (-1, 0) => Some(Dir::West),
            _ => None,
        }
    }
}

/// Which way `to` lies from `from`, for any distance along a single axis.
///
/// Unlike [`Dir::between`] this tolerates the multi-cell jumps that
/// [`simplify`] leaves behind when it collapses collinear runs.
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

/// A rectangular search area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

impl Bounds {
    /// Inclusive containment.
    pub const fn contains(self, point: Point) -> bool {
        point.x >= self.min_x
            && point.x <= self.max_x
            && point.y >= self.min_y
            && point.y <= self.max_y
    }

    /// The diagram's bounding box grown by `margin`, guaranteed to cover
    /// `required` (the endpoints are never outside the search area).
    pub fn around(layout: &DiagramLayout, margin: i32, required: &[Point]) -> Bounds {
        let (x, y, width, height) = layout.bounds;
        let mut bounds = Bounds {
            min_x: x - margin,
            min_y: y - margin,
            max_x: x + width - 1 + margin,
            max_y: y + height - 1 + margin,
        };
        for point in required {
            bounds.min_x = bounds.min_x.min(point.x);
            bounds.min_y = bounds.min_y.min(point.y);
            bounds.max_x = bounds.max_x.max(point.x);
            bounds.max_y = bounds.max_y.max(point.y);
        }
        bounds
    }
}

/// Tuning for the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteOptions {
    /// How far outside the diagram an edge may travel.
    pub margin: i32,
    /// Cost of stepping through a cell covered by a table box.
    pub occupied_cost: u32,
    /// Give up after this many expansions and fall back to a detour.
    pub max_expansions: usize,
}

impl Default for RouteOptions {
    fn default() -> Self {
        RouteOptions {
            margin: 2,
            occupied_cost: 12,
            max_expansions: 60_000,
        }
    }
}

/// One routed foreign key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRoute {
    /// Index into [`Schema::foreign_keys`].
    pub foreign_key: usize,
    /// Index of the referencing table.
    pub from_table: usize,
    /// Index of the referenced table.
    pub to_table: usize,
    /// Corner points from the referencing column to the referenced column.
    pub points: Vec<Point>,
    /// Whether A* gave up and this is a synthetic detour.
    pub fallback: bool,
}

impl EdgeRoute {
    /// First point, if the route has one.
    pub fn start(&self) -> Option<Point> {
        self.points.first().copied()
    }

    /// Last point, if the route has one.
    pub fn end(&self) -> Option<Point> {
        self.points.last().copied()
    }

    /// `(point, character)` for every cell of the route.
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

    /// `(point, character)` for every cell of the route, straight runs included.
    ///
    /// [`EdgeRoute::path_chars`] emits one glyph per *corner*, so the cells
    /// between corners are left blank and the connector looks like scattered
    /// corners. This walks the full unit-step path instead, so every cell of a
    /// straight run is filled and the line is continuous.
    pub fn expanded_path_chars(&self) -> Vec<(Point, char)> {
        let cells = expand(&self.points);
        let mut out = Vec::with_capacity(cells.len());
        for index in 0..cells.len() {
            let point = cells[index];
            let incoming = if index == 0 {
                None
            } else {
                step_dir(point, cells[index - 1])
            };
            let outgoing = if index + 1 == cells.len() {
                None
            } else {
                step_dir(point, cells[index + 1])
            };
            out.push((point, connector_char(incoming, outgoing)));
        }
        out
    }
}

/// The box character for a cell with the given connections.
///
/// A cell with two connections is a corner or a straight run; a cell with one
/// (an endpoint) is a straight stub.
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
                // A reversed segment leaves the direction unchanged.
                (first, second) if first == second => straight_char(first),
                _ => '·',
            }
        }
        (Some(dir), None) | (None, Some(dir)) => straight_char(dir),
        (None, None) => '·',
    }
}

/// Sort key giving the fixed `N, E, S, W` order used by [`connector_char`].
const fn rank(dir: &Dir) -> u8 {
    match dir {
        Dir::North => 0,
        Dir::East => 1,
        Dir::South => 2,
        Dir::West => 3,
    }
}

const fn straight_char(dir: Dir) -> char {
    match dir {
        Dir::East | Dir::West => '─',
        Dir::North | Dir::South => '│',
    }
}

/// Drop the interior points of straight runs, keeping only the corners.
pub fn simplify(points: &[Point]) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len());
    out.push(points[0]);
    for index in 1..points.len() - 1 {
        let previous = points[index - 1];
        let current = points[index];
        let next = points[index + 1];
        let before = Dir::between(previous, current);
        let after = Dir::between(current, next);
        if before != after {
            out.push(current);
        }
    }
    out.push(points[points.len() - 1]);
    out
}

/// Expand a corner polyline back into the unit steps it stands for.
pub fn expand(points: &[Point]) -> Vec<Point> {
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from.x == to.x {
            let step = if to.y >= from.y { 1 } else { -1 };
            let mut y = from.y;
            while y != to.y {
                out.push(Point::new(from.x, y));
                y += step;
            }
        } else if from.y == to.y {
            let step = if to.x >= from.x { 1 } else { -1 };
            let mut x = from.x;
            while x != to.x {
                out.push(Point::new(x, from.y));
                x += step;
            }
        } else {
            // Not orthogonal: bail out instead of inventing a path.
            return out;
        }
    }
    if let Some(last) = points.last() {
        out.push(*last);
    }
    out
}

/// Which cells are covered by a table box.
struct Occupancy {
    rects: Vec<(i32, i32, i32, i32)>,
}

impl Occupancy {
    fn new(layout: &DiagramLayout) -> Self {
        Occupancy {
            rects: layout
                .placements
                .iter()
                .map(|placement| placement.rect())
                .collect(),
        }
    }

    fn contains(&self, point: Point) -> bool {
        self.rects.iter().any(|&(rx, ry, rw, rh)| {
            point.x >= rx && point.x < rx + rw && point.y >= ry && point.y < ry + rh
        })
    }
}

/// A* over the integer grid. `occupied` marks the cells that cost extra.
///
/// Returns the unit-step path, or `None` when the goal is unreachable within the
/// search budget.
pub fn astar(
    start: Point,
    goal: Point,
    occupied: &dyn Fn(Point) -> bool,
    bounds: Bounds,
    options: RouteOptions,
) -> Option<Vec<Point>> {
    if start == goal {
        return Some(vec![start]);
    }

    let mut best: HashMap<Point, u32> = HashMap::new();
    let mut came_from: HashMap<Point, Point> = HashMap::new();
    let mut open: BinaryHeap<std::cmp::Reverse<(u32, i32, i32)>> = BinaryHeap::new();

    best.insert(start, 0);
    open.push(std::cmp::Reverse((start.manhattan(goal), start.y, start.x)));

    let mut expansions = 0usize;
    while let Some(std::cmp::Reverse((_, y, x))) = open.pop() {
        let current = Point::new(x, y);
        if current == goal {
            return Some(reconstruct(&came_from, start, goal));
        }
        expansions += 1;
        if expansions > options.max_expansions {
            return None;
        }
        let travelled = best.get(&current).copied().unwrap_or(0);
        for direction in Dir::ALL {
            let next = current.step(direction);
            if !bounds.contains(next) {
                continue;
            }
            let step_cost = if next == goal {
                0
            } else if occupied(next) {
                options.occupied_cost
            } else {
                1
            };
            let tentative = travelled + step_cost;
            let known = best.get(&next).copied();
            if known.is_none_or(|known| tentative < known) {
                best.insert(next, tentative);
                came_from.insert(next, current);
                open.push(std::cmp::Reverse((
                    tentative + next.manhattan(goal),
                    next.y,
                    next.x,
                )));
            }
        }
    }
    None
}

fn reconstruct(came_from: &HashMap<Point, Point>, start: Point, goal: Point) -> Vec<Point> {
    let mut path = vec![goal];
    let mut cursor = goal;
    while cursor != start {
        match came_from.get(&cursor) {
            Some(previous) => {
                cursor = *previous;
                path.push(cursor);
            }
            None => break,
        }
    }
    path.reverse();
    path
}

/// Which sides an edge should leave and enter the boxes on.
fn anchor_sides(child: &Placement, parent: &Placement) -> (Dir, Dir) {
    let horizontal_overlap = child.x <= parent.right() && parent.x <= child.right();
    let child_middle = child.center().1;
    let parent_middle = parent.center().1;
    if horizontal_overlap && child_middle != parent_middle {
        if child_middle < parent_middle {
            (Dir::South, Dir::North)
        } else {
            (Dir::North, Dir::South)
        }
    } else if child.center().0 <= parent.center().0 {
        (Dir::East, Dir::West)
    } else {
        (Dir::West, Dir::East)
    }
}

/// The world point where an edge touches a box on `side`.
fn anchor(placement: &Placement, side: Dir, column: Option<usize>) -> Point {
    let (x, y) = match side {
        Dir::East => placement.anchor_right(column),
        Dir::West => placement.anchor_left(column),
        Dir::South => placement.anchor_bottom(),
        Dir::North => placement.anchor_top(),
    };
    Point::new(x, y)
}

/// Route every foreign key of `schema` against an already computed layout.
///
/// Foreign keys naming tables that are not in the layout are skipped, so a
/// partially introspected schema still renders.
pub fn route_edges(
    schema: &Schema,
    layout: &DiagramLayout,
    options: RouteOptions,
) -> Vec<EdgeRoute> {
    let occupancy = Occupancy::new(layout);
    // Cells claimed by an already routed edge. Treated exactly like box cells
    // by the router, so later edges strongly prefer a distinct path instead of
    // drawing straight over an earlier one.
    let mut used: HashSet<Point> = HashSet::new();
    let mut routes = Vec::new();

    for (index, foreign_key) in schema.foreign_keys.iter().enumerate() {
        let Some(from_table) = schema.table_index(&foreign_key.from_table) else {
            continue;
        };
        let Some(to_table) = schema.table_index(&foreign_key.to_table) else {
            continue;
        };
        let (Some(child), Some(parent)) =
            (layout.placement(from_table), layout.placement(to_table))
        else {
            continue;
        };

        let from_column = schema.tables[from_table].column_index(&foreign_key.from_columns[0]);
        let to_column = schema.tables[to_table].column_index(&foreign_key.to_columns[0]);

        // A table referencing itself anchors both ends on the same side, so A*
        // draws a compact U-loop hugging that border instead of a full circuit
        // around the box whose far end lands on the referenced column.
        let (child_side, parent_side) = if from_table == to_table {
            (Dir::East, Dir::East)
        } else {
            anchor_sides(child, parent)
        };
        let mut start = anchor(child, child_side, from_column);
        let mut goal = anchor(parent, parent_side, to_column);
        // A borderless box has no edge to merge into, so meeting it on the box
        // cell would overwrite a title character with a junction glyph. Anchor
        // one cell outside instead; the stub then reads as a line reaching the
        // name rather than scribbling over it.
        if layout.params.border.is_none() {
            start = start.step(child_side);
            goal = goal.step(parent_side);
        }
        // A key pointing at its own cell has nowhere to go; drawing it would
        // leave a lone glyph reaching no table.
        if start == goal {
            continue;
        }

        let bounds = Bounds::around(layout, options.margin, &[start, goal]);
        let (points, fallback) = {
            let is_occupied = |point: Point| occupancy.contains(point) || used.contains(&point);
            match astar(start, goal, &is_occupied, bounds, options) {
                Some(path) => (simplify(&path), false),
                None => (margin_detour(start, goal, bounds, &is_occupied), true),
            }
        };
        // A fallback that failed to find a box-free route is dropped rather than
        // drawn as a line that reaches no table.
        if points.len() < 2 || points.first() == points.last() {
            continue;
        }

        // Claim this route's interior cells so the next edge routes around
        // them. The endpoints are the box-border junctions and stay free: two
        // foreign keys may legally land on the same border cell.
        for cell in expand(&points) {
            if cell != start && cell != goal {
                used.insert(cell);
            }
        }

        routes.push(EdgeRoute {
            foreign_key: index,
            from_table,
            to_table,
            points,
            fallback,
        });
    }

    routes
}

/// The last-resort path when A* exhausts its budget: find a box-free way out to
/// the ring just outside the layout, travel along it, and come back in.
///
/// A straight ray is not enough — an anchor on an interior box is usually walled
/// in on every side, and the only way out is a gap between two boxes — so each
/// leg is a breadth-first walk through free cells. When an anchor has no
/// box-free way to the ring at all the path comes back empty and the edge is
/// dropped rather than drawn as a decoy line pointing at nothing.
fn margin_detour(
    start: Point,
    goal: Point,
    bounds: Bounds,
    is_occupied: &dyn Fn(Point) -> bool,
) -> Vec<Point> {
    let (Some(from_leg), Some(to_leg)) = (
        free_leg_to_ring(start, bounds, is_occupied),
        free_leg_to_ring(goal, bounds, is_occupied),
    ) else {
        return Vec::new();
    };
    let from = *from_leg.last().expect("the leg ends on the ring");
    let to = *to_leg.last().expect("the leg ends on the ring");
    let mut points = from_leg;
    points.extend(ring_walk(from, to, bounds));
    points.extend(to_leg[..to_leg.len() - 1].iter().rev().copied());
    points.push(goal);
    points.dedup();
    simplify(&points)
}

/// How many free cells a fallback leg may explore before giving up.
const FALLBACK_CELL_BUDGET: usize = 20_000;

/// Walk through free cells from `from` to the box-free ring, breadth first, and
/// return the leg — `from` first, the ring cell last — or `None` when there is
/// no box-free way out.
fn free_leg_to_ring(
    from: Point,
    bounds: Bounds,
    is_occupied: &dyn Fn(Point) -> bool,
) -> Option<Vec<Point>> {
    if on_ring(from, bounds) {
        return Some(vec![from]);
    }
    let mut queue = VecDeque::from([from]);
    let mut seen = HashSet::from([from]);
    let mut came_from = HashMap::new();
    while let Some(point) = queue.pop_front() {
        if on_ring(point, bounds) {
            return Some(trace_leg(point, from, &came_from));
        }
        if seen.len() > FALLBACK_CELL_BUDGET {
            return None;
        }
        for direction in Dir::ALL {
            let next = point.step(direction);
            if bounds.contains(next) && !is_occupied(next) && seen.insert(next) {
                came_from.insert(next, point);
                queue.push_back(next);
            }
        }
    }
    None
}

/// Follow the search parents back from the ring cell to `from`.
fn trace_leg(point: Point, from: Point, came_from: &HashMap<Point, Point>) -> Vec<Point> {
    let mut leg = vec![point];
    let mut cursor = point;
    while cursor != from {
        let previous = came_from[&cursor];
        leg.push(previous);
        cursor = previous;
    }
    leg.reverse();
    leg
}

/// Is `point` on the perimeter of `bounds`, i.e. on the box-free ring?
const fn on_ring(point: Point, bounds: Bounds) -> bool {
    point.x == bounds.min_x
        || point.x == bounds.max_x
        || point.y == bounds.min_y
        || point.y == bounds.max_y
}

/// The clockwise run of ring cells between two ring points.
fn ring_walk(from: Point, to: Point, bounds: Bounds) -> Vec<Point> {
    let mut out = Vec::new();
    let mut cursor = from;
    let perimeter = 2 * ((bounds.max_x - bounds.min_x) + (bounds.max_y - bounds.min_y));
    for _ in 0..perimeter as usize + 8 {
        if cursor == to {
            break;
        }
        cursor = ring_next(cursor, bounds);
        out.push(cursor);
    }
    out
}

/// The next ring cell clockwise from `point`.
const fn ring_next(point: Point, bounds: Bounds) -> Point {
    if point.y == bounds.min_y {
        if point.x < bounds.max_x {
            return Point::new(point.x + 1, point.y);
        }
        return Point::new(point.x, point.y + 1);
    }
    if point.x == bounds.max_x {
        if point.y < bounds.max_y {
            return Point::new(point.x, point.y + 1);
        }
        return Point::new(point.x - 1, point.y);
    }
    if point.y == bounds.max_y {
        if point.x > bounds.min_x {
            return Point::new(point.x - 1, point.y);
        }
        return Point::new(point.x, point.y - 1);
    }
    if point.y > bounds.min_y {
        return Point::new(point.x, point.y - 1);
    }
    Point::new(point.x + 1, point.y)
}

/// The character an edge endpoint should use where it meets a box border.
fn endpoint_char(layout: &DiagramLayout, point: Point, outward: Dir, fallback: char) -> char {
    for placement in &layout.placements {
        if !placement.contains(point.x, point.y) {
            continue;
        }
        // A box cell that is not on an edge is interior, not a junction.
        let on_top = point.y == placement.y;
        let on_bottom = point.y == placement.bottom();
        let on_left = point.x == placement.x;
        let on_right = point.x == placement.right();
        if !(on_top || on_bottom || on_left || on_right) {
            continue;
        }
        // The line lies on the `outward` side of the box, so the prong must
        // point that way — `contains`-based tests cannot tell an edge cell from
        // the interior, and a long border matches both horizontal neighbours.
        return match outward {
            Dir::East if on_right => '├',
            Dir::West if on_left => '┤',
            Dir::South if on_bottom => '┬',
            Dir::North if on_top => '┴',
            // The edge meets a corner or the wrong side of the border; the
            // straight connector merges cleanly there.
            _ => fallback,
        };
    }
    fallback
}

/// Expanded cells a route paints away from its two box-anchor endpoints.
///
/// Two edges may legally terminate on the same border cell, so endpoints are
/// excluded when deciding which routes actually overlap.
fn interior_path_cells(route: &EdgeRoute) -> Vec<Point> {
    let start = route.start();
    let end = route.end();
    expand(&route.points)
        .into_iter()
        .filter(|cell| Some(*cell) != start && Some(*cell) != end)
        .collect()
}

/// Choose a palette slot for every route so that any two routes sharing a
/// painted cell get different colours.
///
/// Each route prefers the slot derived from its foreign-key index and only
/// moves when an earlier, crossing route already claimed that colour. Routes
/// that never overlap keep their stable colour, so a crossing can never be
/// mistaken for a single line.
pub fn assign_edge_colors(routes: &[EdgeRoute], slots: usize) -> Vec<usize> {
    if slots == 0 {
        return vec![0; routes.len()];
    }
    let mut by_cell: HashMap<Point, Vec<usize>> = HashMap::new();
    for (index, route) in routes.iter().enumerate() {
        for cell in interior_path_cells(route) {
            by_cell.entry(cell).or_default().push(index);
        }
    }
    let mut conflicts: Vec<HashSet<usize>> = vec![HashSet::new(); routes.len()];
    for sharing in by_cell.values() {
        if sharing.len() < 2 {
            continue;
        }
        for &a in sharing {
            for &b in sharing {
                if a != b {
                    conflicts[a].insert(b);
                }
            }
        }
    }
    let mut colors = vec![0usize; routes.len()];
    for index in 0..routes.len() {
        let preferred = routes[index].foreign_key % slots;
        let taken: HashSet<usize> = conflicts[index]
            .iter()
            .filter(|other| **other < index)
            .map(|other| colors[*other])
            .collect();
        colors[index] = (0..slots)
            .map(|offset| (preferred + offset) % slots)
            .find(|slot| !taken.contains(slot))
            .unwrap_or(preferred);
    }
    colors
}

/// Draw routed edges into the canvas.
pub fn draw_routes(
    painter: &mut Painter<'_>,
    layout: &DiagramLayout,
    routes: &[EdgeRoute],
    palette: &[Color],
) {
    if palette.is_empty() {
        return;
    }
    let colors = assign_edge_colors(routes, palette.len());
    for (route, slot) in routes.iter().zip(&colors) {
        let style = Style::new().fg(palette[*slot]);
        let chars = route.expanded_path_chars();
        let count = chars.len();
        for (index, (point, ch)) in chars.iter().enumerate() {
            // Only an endpoint is a junction; aim its prong at the
            // neighbouring route cell, which lies outside the box.
            let neighbour = if count >= 2 && index == 0 {
                Some(chars[1].0)
            } else if count >= 2 && index == count - 1 {
                Some(chars[count - 2].0)
            } else {
                None
            };
            let drawn = match neighbour.and_then(|n| step_dir(*point, n)) {
                Some(outward) => endpoint_char(layout, *point, outward, *ch),
                None => *ch,
            };
            painter.put(point.x, point.y, drawn, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{Camera, Canvas};
    use crate::layout::layout_grid;
    use crate::model::{Column, ForeignKey, KeyKind, Table};
    use crate::zoom::ZoomLevel;

    fn table(name: &str, columns: &[&str]) -> Table {
        Table::new(name).with_columns(
            columns
                .iter()
                .map(|name| Column::new(*name, "integer"))
                .collect(),
        )
    }

    fn three_tables() -> Schema {
        let mut schema = Schema::new();
        schema.tables.push(Table::new("users").with_columns(vec![
            Column::new("id", "integer").with_key(KeyKind::Primary),
            Column::new("email", "text"),
        ]));
        schema.tables.push(Table::new("blocker").with_columns(vec![
            Column::new("id", "integer").with_key(KeyKind::Primary),
            Column::new("filler", "text"),
        ]));
        schema.tables.push(Table::new("orders").with_columns(vec![
            Column::new("id", "integer").with_key(KeyKind::Primary),
            Column::new("user_id", "integer"),
        ]));
        schema
            .foreign_keys
            .push(ForeignKey::new("orders", "user_id", "users", "id"));
        schema
    }

    /// A hub stacked above two children, both referencing the hub's `id`, so
    /// both routes converge on the hub's bottom border and have to share the
    /// free corridor between the boxes.
    fn shared_column_stack() -> Schema {
        let mut schema = Schema::new();
        schema.tables.push(table("hub", &["id", "email"]));
        schema.tables.push(table("mid", &["id", "hub_id"]));
        schema.tables.push(table("low", &["id", "hub_id"]));
        schema
            .foreign_keys
            .push(ForeignKey::new("mid", "hub_id", "hub", "id"));
        schema
            .foreign_keys
            .push(ForeignKey::new("low", "hub_id", "hub", "id"));
        schema
    }

    fn laid_out(schema: &Schema, max_width: i32) -> DiagramLayout {
        layout_grid(schema, &ZoomLevel::Normal.params(), max_width)
    }

    #[test]
    fn every_resolvable_foreign_key_gets_a_route() {
        let schema = three_tables();
        let layout = laid_out(&schema, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].from_table, 2);
        assert_eq!(routes[0].to_table, 0);
        assert!(!routes[0].fallback);
        assert!(routes[0].points.len() >= 2);
    }

    #[test]
    fn dangling_foreign_keys_are_not_routed() {
        let mut schema = three_tables();
        schema
            .foreign_keys
            .push(ForeignKey::new("ghost", "x", "users", "id"));
        let layout = laid_out(&schema, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].foreign_key, 0);
    }

    #[test]
    fn routes_are_orthogonal_and_never_cross_a_box() {
        let schema = three_tables();
        let layout = laid_out(&schema, 40);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        let occupancy = Occupancy::new(&layout);
        for route in &routes {
            let cells = expand(&route.points);
            assert!(cells.len() >= 2);
            for pair in cells.windows(2) {
                assert!(
                    Dir::between(pair[0], pair[1]).is_some(),
                    "not orthogonal: {:?} -> {:?}",
                    pair[0],
                    pair[1]
                );
            }
            for cell in cells.iter().skip(1).take(cells.len().saturating_sub(2)) {
                assert!(
                    !occupancy.contains(*cell),
                    "route {:?} crosses a box at {:?}",
                    route.points,
                    cell
                );
            }
        }
    }

    #[test]
    fn simplify_keeps_only_corners() {
        let straight = vec![
            Point::new(0, 0),
            Point::new(1, 0),
            Point::new(2, 0),
            Point::new(3, 0),
        ];
        assert_eq!(
            simplify(&straight),
            vec![Point::new(0, 0), Point::new(3, 0)]
        );
        let corner = vec![
            Point::new(0, 0),
            Point::new(1, 0),
            Point::new(2, 0),
            Point::new(2, 1),
            Point::new(2, 2),
        ];
        assert_eq!(
            simplify(&corner),
            vec![Point::new(0, 0), Point::new(2, 0), Point::new(2, 2)]
        );
    }

    #[test]
    fn expand_rebuilds_every_unit_step() {
        let points = vec![Point::new(0, 0), Point::new(2, 0), Point::new(2, 1)];
        let cells = expand(&points);
        assert_eq!(
            cells,
            vec![
                Point::new(0, 0),
                Point::new(1, 0),
                Point::new(2, 0),
                Point::new(2, 1)
            ]
        );
    }

    #[test]
    fn connector_char_covers_straights_corners_and_ends() {
        assert_eq!(connector_char(Some(Dir::West), Some(Dir::East)), '─');
        assert_eq!(connector_char(Some(Dir::North), Some(Dir::South)), '│');
        assert_eq!(connector_char(Some(Dir::South), Some(Dir::East)), '┌');
        assert_eq!(connector_char(Some(Dir::South), Some(Dir::West)), '┐');
        assert_eq!(connector_char(Some(Dir::North), Some(Dir::East)), '└');
        assert_eq!(connector_char(Some(Dir::North), Some(Dir::West)), '┘');
        assert_eq!(connector_char(Some(Dir::East), None), '─');
        assert_eq!(connector_char(None, Some(Dir::North)), '│');
        assert_eq!(connector_char(None, None), '·');
    }

    #[test]
    fn astar_walks_a_clear_grid_in_a_straight_line() {
        let start = Point::new(0, 0);
        let goal = Point::new(5, 0);
        let options = RouteOptions::default();
        let bounds = Bounds {
            min_x: -1,
            min_y: -1,
            max_x: 6,
            max_y: 6,
        };
        let path = astar(start, goal, &|_| false, bounds, options).expect("reachable");
        assert_eq!(path.len(), 6);
        assert!(path.iter().all(|point| point.y == 0));
    }

    #[test]
    fn astar_routes_around_an_obstacle() {
        let start = Point::new(0, 0);
        let goal = Point::new(4, 0);
        // A wall in the middle forcing a detour over the top.
        let wall = |point: Point| point.x == 2 && point.y != -1;
        let options = RouteOptions::default();
        let bounds = Bounds {
            min_x: -2,
            min_y: -2,
            max_x: 6,
            max_y: 3,
        };
        let path = astar(start, goal, &wall, bounds, options).expect("reachable");
        assert!(
            path.contains(&Point::new(2, -1)),
            "should pass above the wall"
        );
        assert!(!path.contains(&Point::new(2, 0)));
        assert_eq!(path.last(), Some(&goal));
    }

    #[test]
    fn astar_gives_up_when_the_budget_is_exhausted() {
        let options = RouteOptions {
            max_expansions: 0,
            ..RouteOptions::default()
        };
        let bounds = Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 10,
            max_y: 10,
        };
        assert!(astar(
            Point::new(0, 0),
            Point::new(5, 5),
            &|_| false,
            bounds,
            options
        )
        .is_none());
    }

    #[test]
    fn astar_returns_nothing_when_the_goal_is_out_of_bounds() {
        let options = RouteOptions::default();
        let bounds = Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 2,
            max_y: 2,
        };
        assert!(astar(
            Point::new(0, 0),
            Point::new(9, 9),
            &|_| false,
            bounds,
            options
        )
        .is_none());
    }

    #[test]
    fn a_blocked_router_falls_back_to_a_box_free_margin_route() {
        let schema = three_tables();
        let layout = laid_out(&schema, 40);
        let options = RouteOptions {
            max_expansions: 0,
            ..RouteOptions::default()
        };
        let routes = route_edges(&schema, &layout, options);
        assert_eq!(routes.len(), 1);
        assert!(routes[0].fallback);
        let start = routes[0].start().expect("start");
        let goal = routes[0].end().expect("end");
        // A fallback is still a usable orthogonal polyline...
        for pair in routes[0].points.windows(2) {
            assert!(pair[0].x == pair[1].x || pair[0].y == pair[1].y);
        }
        // ...and it reaches the goal along the free margin ring without cutting
        // through any table on the way.
        for (point, _) in routes[0].expanded_path_chars() {
            let inside = layout
                .placements
                .iter()
                .any(|placement| placement.contains(point.x, point.y));
            assert!(
                !inside || point == start || point == goal,
                "fallback crossed a table at {point:?}"
            );
        }
    }

    #[test]
    fn vertical_neighbours_leave_through_the_bottom() {
        let schema = three_tables();
        // Narrow layout: each box gets its own row, so the FK points downwards.
        let layout = laid_out(&schema, 30);
        let child = layout.placement(2).expect("orders");
        let parent = layout.placement(0).expect("users");
        let (child_side, parent_side) = anchor_sides(child, parent);
        if child.y > parent.y && child.x <= parent.right() && parent.x <= child.right() {
            // The child sits below the parent, so the edge leaves the child's
            // top edge and enters the parent's bottom edge.
            assert_eq!(child_side, Dir::North);
            assert_eq!(parent_side, Dir::South);
        }
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        let start = routes[0].start().expect("start");
        // The anchor always sits on the child's outline.
        assert!(
            start.x == child.x
                || start.x == child.right()
                || start.y == child.y
                || start.y == child.bottom()
        );
    }

    #[test]
    fn self_referencing_tables_route_a_u_loop_on_one_side() {
        let mut schema = Schema::new();
        schema
            .tables
            .push(table("categories", &["id", "parent_id"]));
        schema.foreign_keys.push(
            ForeignKey::new("categories", "parent_id", "categories", "id").named("fk_parent"),
        );
        let layout = laid_out(&schema, 80);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(routes.len(), 1);
        assert!(routes[0].points.len() >= 2);
        let categories = layout.placement(0).expect("categories");
        let start = routes[0].start().expect("start");
        let goal = routes[0].end().expect("end");
        // A self-reference leaves and re-enters the same (right) border, on the
        // two different rows of the referenced and referencing columns.
        assert_eq!(start.x, categories.right());
        assert_eq!(goal.x, categories.right());
        assert_ne!(start.y, goal.y);
        // The loop hugs that border and never cuts through the box.
        for pair in routes[0].points.windows(2) {
            assert!(pair[0].x == pair[1].x || pair[0].y == pair[1].y);
        }
        for point in &routes[0].points {
            assert!(
                point.x >= categories.right(),
                "self loop strayed inward to {point:?}"
            );
        }
    }

    #[test]
    fn path_chars_are_connected_box_drawing_characters() {
        let route = EdgeRoute {
            foreign_key: 0,
            from_table: 0,
            to_table: 1,
            points: vec![Point::new(0, 0), Point::new(2, 0), Point::new(2, 2)],
            fallback: false,
        };
        let chars = route.path_chars();
        assert_eq!(chars[0].1, '─');
        assert_eq!(chars[1].1, '┐');
        assert_eq!(chars[2].1, '│');
    }

    #[test]
    fn composite_foreign_keys_anchor_on_the_first_column() {
        let mut schema = Schema::new();
        schema.tables.push(Table::new("parent").with_columns(vec![
            Column::new("a", "integer"),
            Column::new("b", "integer"),
        ]));
        schema.tables.push(Table::new("child").with_columns(vec![
            Column::new("pa", "integer"),
            Column::new("pb", "integer"),
        ]));
        schema.foreign_keys.push(ForeignKey::composite(
            "child",
            vec!["pa".to_string(), "pb".to_string()],
            "parent",
            vec!["a".to_string(), "b".to_string()],
        ));
        let layout = laid_out(&schema, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(routes.len(), 1);
        // The anchor row is the first column of the pair, not the box middle.
        let child = layout.placement(1).expect("child");
        if let Some(row) = child.column_row(0) {
            assert_eq!(routes[0].start().map(|p| p.y), Some(row));
        }
    }

    #[test]
    fn routing_is_deterministic() {
        let schema = three_tables();
        let layout = laid_out(&schema, 40);
        let first = route_edges(&schema, &layout, RouteOptions::default());
        let second = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(first, second);
    }

    #[test]
    fn columns_outside_the_schema_fall_back_to_the_box_middle() {
        let mut schema = Schema::new();
        schema.tables.push(table("users", &["id"]));
        schema.tables.push(table("orders", &["id"]));
        schema
            .foreign_keys
            .push(ForeignKey::new("orders", "missing", "users", "id"));
        let layout = laid_out(&schema, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(routes.len(), 1);
        let child = layout.placement(1).expect("orders");
        assert_eq!(
            routes[0].start().map(|p| p.y),
            Some(child.y + child.layout.middle_row())
        );
    }

    #[test]
    fn expanded_path_chars_fill_every_cell_of_a_straight_run() {
        let route = EdgeRoute {
            foreign_key: 0,
            from_table: 0,
            to_table: 1,
            points: vec![Point::new(0, 0), Point::new(3, 0), Point::new(3, 2)],
            fallback: false,
        };
        let chars = route.expanded_path_chars();
        let cells: Vec<Point> = chars.iter().map(|(point, _)| *point).collect();
        // One cell per unit step, corners included, nothing skipped.
        assert_eq!(
            cells,
            vec![
                Point::new(0, 0),
                Point::new(1, 0),
                Point::new(2, 0),
                Point::new(3, 0),
                Point::new(3, 1),
                Point::new(3, 2),
            ]
        );
        // The cells between the corners used to be blank; they are not now.
        for (point, ch) in &chars {
            assert_ne!(*ch, ' ', "blank connector cell at {point:?}");
        }
    }
    #[test]
    fn draw_routes_paints_an_unbroken_line() {
        let schema = three_tables();
        let layout = laid_out(&schema, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        let mut canvas = Canvas::new(200, 60);
        {
            let mut painter = Painter::new(&mut canvas, Camera::default());
            draw_routes(&mut painter, &layout, &routes, &EDGE_PALETTE);
        }
        for route in &routes {
            for (point, _) in route.expanded_path_chars() {
                let cell = canvas
                    .cell(point.x, point.y)
                    .expect("route cell is on canvas");
                assert_ne!(cell.ch, ' ', "gap in the connector at {point:?}");
            }
        }
    }

    #[test]
    fn endpoint_char_turns_its_prong_away_from_the_box() {
        let schema = three_tables();
        let layout = laid_out(&schema, 200);
        let users = layout.placement(0).expect("users");
        let mid_y = users.y + users.layout.middle_row();
        let mid_x = users.x + users.layout.box_width / 2;

        // A line arriving at a border leaves through it, so the prong points
        // out of the box: right border -> right prong, and so on.
        assert_eq!(
            endpoint_char(&layout, Point::new(users.right(), mid_y), Dir::East, '·'),
            '├'
        );
        assert_eq!(
            endpoint_char(&layout, Point::new(users.x, mid_y), Dir::West, '·'),
            '┤'
        );
        assert_eq!(
            endpoint_char(&layout, Point::new(mid_x, users.bottom()), Dir::South, '·'),
            '┬'
        );
        assert_eq!(
            endpoint_char(&layout, Point::new(mid_x, users.y), Dir::North, '·'),
            '┴'
        );

        // A prong that does not match the border it lands on keeps the route's
        // own connector instead of inventing a junction.
        assert_eq!(
            endpoint_char(&layout, Point::new(users.right(), mid_y), Dir::North, '─'),
            '─'
        );
        // Interior cells are never junctions.
        assert_eq!(
            endpoint_char(
                &layout,
                Point::new(users.x + 1, users.y + 1),
                Dir::East,
                '─'
            ),
            '─'
        );
    }

    #[test]
    fn borderless_boxes_are_met_one_cell_outside() {
        let schema = three_tables();
        let params = ZoomLevel::Out2.params();
        assert!(params.border.is_none());
        let layout = layout_grid(&schema, &params, 200);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert!(!routes.is_empty());
        for route in &routes {
            let child = layout.placement(route.from_table).expect("child");
            let parent = layout.placement(route.to_table).expect("parent");
            let start = route.start().expect("start");
            let goal = route.end().expect("end");
            // The anchor steps off the borderless box so it cannot overwrite a
            // title character, but it stays glued to the edge it left from.
            assert!(
                !child.contains(start.x, start.y),
                "anchor sits on the child box"
            );
            assert!(
                !parent.contains(goal.x, goal.y),
                "anchor sits on the parent box"
            );
            for (anchor, box_) in [(&start, child), (&goal, parent)] {
                let touches = [Dir::North, Dir::East, Dir::South, Dir::West]
                    .iter()
                    .any(|dir| box_.contains(anchor.step(*dir).x, anchor.step(*dir).y));
                assert!(touches, "anchor {anchor:?} is not adjacent to its box");
            }
        }
    }

    #[test]
    fn a_route_onto_the_same_column_is_skipped() {
        let mut schema = Schema::new();
        schema
            .tables
            .push(table("categories", &["id", "parent_id"]));
        schema.foreign_keys.push(ForeignKey::new(
            "categories",
            "parent_id",
            "categories",
            "parent_id",
        ));
        let layout = laid_out(&schema, 80);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert!(
            routes.is_empty(),
            "a zero-length self reference drew a route: {routes:?}"
        );
    }

    #[test]
    fn a_fallback_without_a_free_ring_entry_draws_nothing() {
        let bounds = Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 10,
            max_y: 10,
        };
        let blocked = |_point: Point| true;
        let path = margin_detour(Point::new(5, 5), Point::new(5, 9), bounds, &blocked);
        assert!(
            path.is_empty(),
            "a blocked fallback should be dropped, not drawn: {path:?}"
        );
    }

    #[test]
    fn draw_routes_colors_each_connection_by_its_foreign_key() {
        let layout = laid_out(&three_tables(), 200);
        let route = |foreign_key: usize, x: i32| EdgeRoute {
            foreign_key,
            from_table: 0,
            to_table: 1,
            points: vec![Point::new(x, 5), Point::new(x, 12)],
            fallback: false,
        };
        let routes = vec![route(0, 5), route(1, 15)];
        let mut canvas = Canvas::new(40, 20);
        {
            let mut painter = Painter::new(&mut canvas, Camera::default());
            draw_routes(&mut painter, &layout, &routes, &EDGE_PALETTE);
        }
        let first = canvas.cell(5, 5).expect("first cell").style.fg;
        let second = canvas.cell(15, 5).expect("second cell").style.fg;
        assert_eq!(first, Some(EDGE_PALETTE[0]));
        assert_eq!(second, Some(EDGE_PALETTE[1]));
        assert_ne!(first, second);

        // The palette wraps, and a foreign key keeps its colour regardless of
        // where its route sits in the list.
        let mut canvas = Canvas::new(40, 20);
        {
            let mut painter = Painter::new(&mut canvas, Camera::default());
            draw_routes(
                &mut painter,
                &layout,
                &[route(EDGE_PALETTE.len(), 25)],
                &EDGE_PALETTE,
            );
        }
        assert_eq!(
            canvas.cell(25, 5).expect("wrapped cell").style.fg,
            Some(EDGE_PALETTE[0])
        );
    }

    #[test]
    fn crossing_edges_never_share_a_colour() {
        let layout = laid_out(&three_tables(), 200);
        let route = |foreign_key: usize, points: Vec<Point>| EdgeRoute {
            foreign_key,
            from_table: 0,
            to_table: 1,
            points,
            fallback: false,
        };
        // A vertical and a horizontal line that cross in free space, with
        // foreign keys whose preferred slot is the same.
        let routes = vec![
            route(0, vec![Point::new(5, 5), Point::new(5, 14)]),
            route(
                EDGE_PALETTE.len(),
                vec![Point::new(1, 9), Point::new(12, 9)],
            ),
        ];
        assert_eq!(
            routes[0].foreign_key % EDGE_PALETTE.len(),
            routes[1].foreign_key % EDGE_PALETTE.len(),
            "the two foreign keys would collide without conflict-aware slots"
        );

        assert_ne!(
            assign_edge_colors(&routes, EDGE_PALETTE.len())[0],
            assign_edge_colors(&routes, EDGE_PALETTE.len())[1],
        );

        let mut canvas = Canvas::new(40, 20);
        {
            let mut painter = Painter::new(&mut canvas, Camera::default());
            draw_routes(&mut painter, &layout, &routes, &EDGE_PALETTE);
        }
        let vertical = canvas.cell(5, 6).expect("vertical").style.fg;
        let horizontal = canvas.cell(2, 9).expect("horizontal").style.fg;
        assert_eq!(vertical, Some(EDGE_PALETTE[0]));
        assert_ne!(vertical, horizontal);
    }

    #[test]
    fn non_crossing_edges_may_share_a_colour() {
        let route = |foreign_key: usize, x: i32| EdgeRoute {
            foreign_key,
            from_table: 0,
            to_table: 1,
            points: vec![Point::new(x, 5), Point::new(x, 12)],
            fallback: false,
        };
        let routes = vec![route(0, 5), route(EDGE_PALETTE.len(), 15)];
        assert_eq!(assign_edge_colors(&routes, EDGE_PALETTE.len()), vec![0, 0]);
    }

    /// Expanded cells a route actually traverses in free space: the whole
    /// path minus the two border-junction endpoints, which two edges may
    /// legally land on together.
    fn interior_cells(route: &EdgeRoute) -> HashSet<Point> {
        let start = route.start().expect("start");
        let end = route.end().expect("end");
        expand(&route.points)
            .into_iter()
            .filter(|cell| *cell != start && *cell != end)
            .collect()
    }

    #[test]
    fn routes_to_a_shared_column_do_not_override_each_other() {
        let schema = shared_column_stack();
        // A narrow frame forces one column, so both routes climb the same gap
        // between the boxes; the second has to swing aside instead of drawing
        // straight over the first.
        let layout = laid_out(&schema, 30);
        let routes = route_edges(&schema, &layout, RouteOptions::default());
        assert_eq!(
            routes.len(),
            2,
            "both foreign keys should route: {routes:?}"
        );

        let first = interior_cells(&routes[0]);
        let second = interior_cells(&routes[1]);
        let shared: Vec<_> = first.intersection(&second).collect();
        assert!(
            shared.is_empty(),
            "routes to the same column share cells: {shared:?}"
        );
    }

    #[test]
    fn overlap_avoidance_is_deterministic() {
        let schema = shared_column_stack();
        let layout = laid_out(&schema, 30);
        let first = route_edges(&schema, &layout, RouteOptions::default());
        let second = route_edges(&schema, &layout, RouteOptions::default());
        let points: Vec<Vec<Point>> = first.iter().map(|route| route.points.clone()).collect();
        let again: Vec<Vec<Point>> = second.iter().map(|route| route.points.clone()).collect();
        assert_eq!(points, again, "routing must stay reproducible");
    }
}
