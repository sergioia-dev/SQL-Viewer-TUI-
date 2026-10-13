//! Placing measured table boxes on the integer world grid.
//!
//! The layout is deterministic: tables are placed row-major in schema order,
//! wrapping to a new row when the next box would cross `max_width`. A
//! force-directed layout was rejected for v1 because it makes screenshots,
//! tests and muscle memory unstable — the same schema must always look the same.
//!
//! Because zoom changes box sizes, the layout is recomputed on every zoom change
//! rather than cached; it is cheap (linear in the number of tables).

use crate::model::Schema;
use crate::table_view::{self, TableLayout};
use crate::zoom::ZoomParams;

/// One table box placed at a world position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Index into [`Schema::tables`].
    pub table: usize,
    /// Measured geometry, relative to [`Placement::x`]/[`Placement::y`].
    pub layout: TableLayout,
    /// World column of the box's left edge.
    pub x: i32,
    /// World row of the box's top edge.
    pub y: i32,
}

impl Placement {
    /// Place a measured box at a world position.
    pub fn new(table: usize, layout: TableLayout, x: i32, y: i32) -> Self {
        Placement {
            table,
            layout,
            x,
            y,
        }
    }

    /// World column of the right edge.
    pub fn right(&self) -> i32 {
        self.x + self.layout.box_width - 1
    }

    /// World row of the bottom edge.
    pub fn bottom(&self) -> i32 {
        self.y + self.layout.box_height - 1
    }

    /// World `(x, y, width, height)`.
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        (
            self.x,
            self.y,
            self.layout.box_width,
            self.layout.box_height,
        )
    }

    /// World middle of the box.
    pub fn center(&self) -> (i32, i32) {
        (
            self.x + self.layout.box_width / 2,
            self.y + self.layout.box_height / 2,
        )
    }

    /// World row of the `index`th column's name line.
    pub fn column_row(&self, index: usize) -> Option<i32> {
        self.layout.column_row(index).map(|offset| self.y + offset)
    }

    /// Where an edge attaches to the left border. `column` is the column whose
    /// row the edge should meet, falling back to the middle of the box.
    pub fn anchor_left(&self, column: Option<usize>) -> (i32, i32) {
        (
            self.x,
            column
                .and_then(|index| self.column_row(index))
                .unwrap_or_else(|| self.y + self.layout.middle_row()),
        )
    }

    /// Where an edge attaches to the right border.
    pub fn anchor_right(&self, column: Option<usize>) -> (i32, i32) {
        (
            self.right(),
            column
                .and_then(|index| self.column_row(index))
                .unwrap_or_else(|| self.y + self.layout.middle_row()),
        )
    }

    /// Where a vertical edge attaches to the top border. Columns are laid out
    /// across the box rather than down it, so this is the box's middle column.
    pub fn anchor_top(&self) -> (i32, i32) {
        (self.middle_column(), self.y)
    }

    /// Where a vertical edge attaches to the bottom border.
    pub fn anchor_bottom(&self) -> (i32, i32) {
        (self.middle_column(), self.bottom())
    }

    /// World column of the box's middle.
    pub fn middle_column(&self) -> i32 {
        self.x + self.layout.box_width / 2
    }

    /// Whether a world point is inside the box.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        let (bx, by, bw, bh) = self.rect();
        x >= bx && y >= by && x < bx + bw && y < by + bh
    }

    /// Index of the column whose rendered line is `y`, if any.
    pub fn column_at_row(&self, y: i32) -> Option<usize> {
        let relative = y - self.y;
        self.layout.rows.iter().find_map(|row| match row.kind {
            table_view::RowKind::ColumnName { index } if row.y == relative => Some(index),
            _ => None,
        })
    }
}

/// Every box placed, plus the area they cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramLayout {
    pub placements: Vec<Placement>,
    /// The zoom parameters this layout was measured with.
    pub params: ZoomParams,
    /// Bounding box of all placements: `(x, y, width, height)`.
    pub bounds: (i32, i32, i32, i32),
}

impl DiagramLayout {
    /// Placement of the `index`th table.
    pub fn placement(&self, index: usize) -> Option<&Placement> {
        self.placements.iter().find(|p| p.table == index)
    }

    /// Index of the topmost table containing a world point.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<usize> {
        self.placements
            .iter()
            .rev()
            .find(|placement| placement.contains(x, y))
            .map(|placement| placement.table)
    }

    /// Width of the covered area.
    pub fn width(&self) -> i32 {
        self.bounds.2
    }

    /// Height of the covered area.
    pub fn height(&self) -> i32 {
        self.bounds.3
    }

    /// Scroll offset that puts the covered area's centre in the viewport centre.
    pub fn centroid(&self) -> (i32, i32) {
        let (x, y, w, h) = self.bounds;
        (x + w / 2, y + h / 2)
    }
}

/// Place every table of `schema` row-major within `max_width` columns.
///
/// A box wider than `max_width` is still placed — alone on its own row — rather
/// than being dropped or split.
pub fn layout_grid(schema: &Schema, params: &ZoomParams, max_width: i32) -> DiagramLayout {
    let max_width = max_width.max(1);
    let mut placements = Vec::with_capacity(schema.tables.len());
    let mut width = 0;
    let mut height = 0;

    for (index, table) in schema.tables.iter().enumerate() {
        let layout = table_view::measure(table, params);
        let (x, y) = next_position(&placements, &layout, params, max_width);
        width = width.max(x + layout.box_width);
        height = height.max(y + layout.box_height);
        placements.push(Placement::new(index, layout, x, y));
    }

    DiagramLayout {
        placements,
        params: *params,
        bounds: (0, 0, width, height),
    }
}

/// Where the next box goes: appended to the current row, or wrapped onto a
/// fresh row when it would cross `max_width`.
fn next_position(
    placements: &[Placement],
    layout: &TableLayout,
    params: &ZoomParams,
    max_width: i32,
) -> (i32, i32) {
    match placements.last() {
        None => (0, 0),
        Some(last) => {
            let x = last.x + last.layout.box_width + params.box_gap_x as i32;
            if x + layout.box_width > max_width {
                // `row_bottom` is the inclusive bottom edge, so `+ 1` steps
                // past it; `box_gap_y` is then the true number of blank lines.
                (0, row_bottom(placements) + 1 + params.box_gap_y as i32)
            } else {
                (x, last.y)
            }
        }
    }
}

/// Bottom edge of the row the last box is on.
fn row_bottom(placements: &[Placement]) -> i32 {
    let Some(last) = placements.last() else {
        return 0;
    };
    placements
        .iter()
        .filter(|placement| placement.y == last.y)
        .map(|placement| placement.bottom())
        .max()
        .unwrap_or(last.bottom())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, ForeignKey, KeyKind, Table};
    use crate::zoom::ZoomLevel;

    fn schema() -> Schema {
        let mut schema = Schema::new();
        for name in ["users", "orders", "order_items", "products"] {
            schema.tables.push(Table::new(name).with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("payload", "text"),
            ]));
        }
        schema
            .foreign_keys
            .push(ForeignKey::new("orders", "user_id", "users", "id"));
        schema
    }

    #[test]
    fn every_table_is_placed_once_in_schema_order() {
        let layout = layout_grid(&schema(), &ZoomLevel::Normal.params(), 200);
        assert_eq!(layout.placements.len(), 4);
        for (position, placement) in layout.placements.iter().enumerate() {
            assert_eq!(placement.table, position);
        }
    }

    #[test]
    fn layout_is_deterministic() {
        let params = ZoomLevel::Normal.params();
        let first = layout_grid(&schema(), &params, 80);
        let second = layout_grid(&schema(), &params, 80);
        assert_eq!(first.placements, second.placements);
        assert_eq!(first.bounds, second.bounds);
    }

    #[test]
    fn boxes_never_overlap() {
        for level in ZoomLevel::ALL {
            let params = level.params();
            let layout = layout_grid(&schema(), &params, 70);
            for (i, a) in layout.placements.iter().enumerate() {
                for b in layout.placements.iter().skip(i + 1) {
                    let (ax, ay, aw, ah) = a.rect();
                    let (bx, by, bw, bh) = b.rect();
                    let overlap = ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah;
                    assert!(!overlap, "{:?} overlaps {:?}", a.rect(), b.rect());
                }
            }
        }
    }

    #[test]
    fn boxes_wrap_to_a_new_row_when_the_width_is_exceeded() {
        let params = ZoomLevel::Normal.params();
        let narrow = layout_grid(&schema(), &params, 30);
        let rows: Vec<i32> = narrow.placements.iter().map(|p| p.y).collect();
        assert!(rows.iter().any(|y| *y > 0), "expected a wrapped row");
        assert!(narrow.placements.windows(2).all(|w| w[0].y <= w[1].y));
    }

    #[test]
    fn gap_between_neighbours_is_honoured() {
        // A narrow grid forces wraps, so both the horizontal gap inside a row
        // and the vertical gap between rows are exercised.
        for level in [ZoomLevel::Normal, ZoomLevel::Out2] {
            let params = level.params();
            for max_width in [500, 30] {
                let layout = layout_grid(&schema(), &params, max_width);
                for pair in layout.placements.windows(2) {
                    if pair[1].y == pair[0].y {
                        // `right()` is the inclusive last column, so the number
                        // of free cells between the boxes is `next - right - 1`.
                        assert_eq!(pair[1].x - pair[0].right() - 1, params.box_gap_x as i32);
                    } else {
                        // `bottom()` is the inclusive last row, so the number of
                        // free lines between the rows is `next - bottom - 1`.
                        let above = layout
                            .placements
                            .iter()
                            .filter(|p| p.y == pair[0].y)
                            .map(|p| p.bottom())
                            .max()
                            .expect("the wrapped-from row is not empty");
                        assert_eq!(pair[1].y - above - 1, params.box_gap_y as i32);
                    }
                }
                if max_width == 30 {
                    assert!(
                        layout.placements.windows(2).any(|p| p[1].y != p[0].y),
                        "a narrow grid must wrap to a second row"
                    );
                }
            }
        }
    }

    #[test]
    fn a_single_wide_table_still_gets_placed() {
        let params = ZoomLevel::In1.params();
        let layout = layout_grid(&schema(), &params, 5);
        assert_eq!(layout.placements.len(), 4);
        assert!(layout.placements.iter().all(|p| p.x == 0));
    }

    #[test]
    fn bounds_cover_every_box() {
        let params = ZoomLevel::Normal.params();
        let layout = layout_grid(&schema(), &params, 60);
        let (bx, by, bw, bh) = layout.bounds;
        for placement in &layout.placements {
            let (x, y, w, h) = placement.rect();
            assert!(x >= bx && y >= by);
            assert!(x + w <= bx + bw && y + h <= by + bh);
        }
        assert_eq!(layout.width(), bw);
        assert_eq!(layout.height(), bh);
    }

    #[test]
    fn hit_test_finds_the_box_under_a_point() {
        let layout = layout_grid(&schema(), &ZoomLevel::Normal.params(), 200);
        let first = &layout.placements[0];
        let (x, y) = first.center();
        assert_eq!(layout.hit_test(x, y), Some(0));
        assert_eq!(layout.hit_test(-50, -50), None);
    }

    #[test]
    fn anchors_sit_on_the_border_at_the_column_row() {
        let layout = layout_grid(&schema(), &ZoomLevel::Normal.params(), 200);
        let placement = &layout.placements[0];
        let (left_x, left_y) = placement.anchor_left(Some(1));
        assert_eq!(left_x, placement.x);
        assert_eq!(left_y, placement.column_row(1).unwrap());
        let (right_x, right_y) = placement.anchor_right(None);
        assert_eq!(right_x, placement.right());
        assert_eq!(right_y, placement.y + placement.layout.middle_row());
    }

    #[test]
    fn column_at_row_maps_lines_back_to_columns() {
        let layout = layout_grid(&schema(), &ZoomLevel::Normal.params(), 200);
        let placement = &layout.placements[0];
        let row = placement.column_row(1).unwrap();
        assert_eq!(placement.column_at_row(row), Some(1));
        assert_eq!(placement.column_at_row(placement.y), None);
    }

    #[test]
    fn empty_schema_has_empty_bounds() {
        let layout = layout_grid(&Schema::new(), &ZoomLevel::Normal.params(), 80);
        assert!(layout.placements.is_empty());
        assert_eq!(layout.bounds, (0, 0, 0, 0));
    }
}
