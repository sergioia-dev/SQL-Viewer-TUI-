//! Measuring and drawing a single table box.
//!
//! The box is drawn straight into our own [`Canvas`](crate::canvas::Canvas) — the
//! `Block` widget cannot express the level-of-detail borders (none at `Out2`,
//! double at `In1`) together with the anchors foreign key edges attach to, so the
//! box characters come from [`BorderChars`] and every line is placed by hand.
//!
//! Measuring and drawing are separate: [`TableLayout`] carries the geometry
//! (relative to the box's top left corner) and [`draw`] replays it. The layout
//! pass needs the geometry without a painter, and the anchors in
//! [`TableLayout::column_row`] are what the edge router connects to.

use crate::canvas::Painter;
use crate::model::Table;
use crate::zoom::ZoomParams;

/// Which part of a line a piece of text is, so it can be styled on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRole {
    /// The table name in the title row.
    Title,
    /// A column name.
    Name,
    /// A column's type.
    Type,
    /// The `PK` / `FK` / `PK,FK` chip.
    Flag,
    /// Nullability and default text at the closest zoom levels.
    Detail,
    /// Alignment filler; carries no style.
    Padding,
}

/// A piece of a rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub role: SegmentRole,
    pub text: String,
}

impl Segment {
    fn new(role: SegmentRole, text: impl Into<String>) -> Self {
        Segment {
            role,
            text: text.into(),
        }
    }

    /// Display width in terminal columns.
    fn width(&self) -> i32 {
        self.text.chars().count() as i32
    }
}

/// What a rendered line inside a box is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The table name.
    Title,
    /// The first line of a column: its name.
    ColumnName { index: usize },
    /// The second line of a column, at [`ZoomLevel::In1`](crate::zoom::ZoomLevel::In1).
    ColumnDetail { index: usize },
}

/// One line inside a box, positioned relative to the box's top left corner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// Line offset from the top of the box, including border and padding.
    pub y: i32,
    pub segments: Vec<Segment>,
}

impl Row {
    /// Display width of the line, ignoring trailing filler.
    fn width(&self) -> i32 {
        let mut width = 0;
        for segment in &self.segments {
            width += segment.width();
        }
        width
    }
}

/// Geometry of one table box, in terminal cells, relative to the box origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLayout {
    /// Lines in draw order, with their offsets.
    pub rows: Vec<Row>,
    /// Whether a title row was emitted.
    pub has_title: bool,
    /// Width of the widest line.
    pub content_width: i32,
    /// Number of lines.
    pub content_height: i32,
    /// Column where the type text starts (0 when types are hidden).
    pub type_column: i32,
    /// Column where the key chip starts (0 when chips are hidden).
    pub flag_column: i32,
    /// Full box width including border and padding.
    pub box_width: i32,
    /// Full box height including border and padding.
    pub box_height: i32,
}

impl TableLayout {
    /// Offset of the first line of `index`th column, if it is drawn at all.
    pub fn column_row(&self, index: usize) -> Option<i32> {
        self.rows.iter().find_map(|row| match row.kind {
            RowKind::ColumnName { index: i } if i == index => Some(row.y),
            _ => None,
        })
    }

    /// Offset of the box's vertical middle, used when an edge lands on a box
    /// rather than on a specific column.
    pub fn middle_row(&self) -> i32 {
        self.box_height / 2
    }

    /// Whether `x`/`y`, relative to the box origin, is inside the box.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.box_width && y < self.box_height
    }
}

/// Measure a table and produce the lines to draw.
pub fn measure(table: &Table, params: &ZoomParams) -> TableLayout {
    let mut rows: Vec<Row> = Vec::new();
    let title = title_text(table, params);
    rows.push(Row {
        kind: RowKind::Title,
        y: 0,
        segments: vec![Segment::new(SegmentRole::Title, title)],
    });

    let mut type_column = 0;
    let mut flag_column = 0;
    if params.show_columns {
        let names: Vec<String> = table
            .columns
            .iter()
            .map(|column| params.fit(&column.name))
            .collect();
        let types: Vec<String> = table
            .columns
            .iter()
            .map(|column| params.fit(&column.data_type))
            .collect();

        let name_width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        let type_width = if params.show_types {
            types.iter().map(|t| t.chars().count()).max().unwrap_or(0)
        } else {
            0
        };
        type_column = if params.show_types {
            name_width as i32 + 2
        } else {
            0
        };
        flag_column = if params.show_flags {
            (name_width + if params.show_types { 2 + type_width } else { 0 }) as i32 + 2
        } else {
            0
        };

        for (index, column) in table.columns.iter().enumerate() {
            let mut segments = vec![Segment::new(SegmentRole::Name, names[index].clone())];
            if params.show_types {
                push_padded(&mut segments, name_width - names[index].chars().count() + 2);
                segments.push(Segment::new(SegmentRole::Type, types[index].clone()));
            }
            if params.show_flags {
                push_padded(
                    &mut segments,
                    type_width.saturating_sub(types[index].chars().count()) + 2,
                );
                segments.push(Segment::new(SegmentRole::Flag, column.key.marker()));
            }
            rows.push(Row {
                kind: RowKind::ColumnName { index },
                y: 0,
                segments,
            });

            if params.two_line_columns {
                let mut detail = vec![Segment::new(SegmentRole::Type, types[index].clone())];
                if params.show_nullability {
                    detail.push(Segment::new(SegmentRole::Padding, " · "));
                    detail.push(Segment::new(
                        SegmentRole::Detail,
                        column.nullability_label(),
                    ));
                }
                if params.show_defaults {
                    if let Some(default) = &column.default {
                        detail.push(Segment::new(SegmentRole::Padding, " · = "));
                        detail.push(Segment::new(
                            SegmentRole::Detail,
                            params.fit(&one_line(default)),
                        ));
                    }
                }
                rows.push(Row {
                    kind: RowKind::ColumnDetail { index },
                    y: 0,
                    segments: detail,
                });
            }
        }
    }

    let content_width = rows.iter().map(|row| row.width()).max().unwrap_or(0);
    let content_height = rows.len() as i32;
    let box_width = content_width + params.border_width() + 2 * params.padding_x as i32;
    let box_height = content_height + params.border_height() + 2 * params.padding_y as i32;

    // Assign vertical offsets now that the box height is known.
    for (offset, row) in rows.iter_mut().enumerate() {
        row.y = params.inner_y() + offset as i32;
    }

    TableLayout {
        rows,
        has_title: true,
        content_width,
        content_height,
        type_column,
        flag_column,
        box_width,
        box_height,
    }
}

/// Draw a measured table at world `(x, y)`.
///
/// The border is drawn last so that it is never overwritten by a line that
/// happens to reach the edge.
pub fn draw(painter: &mut Painter<'_>, layout: &TableLayout, x: i32, y: i32, params: &ZoomParams) {
    for row in &layout.rows {
        let mut column = x + params.inner_x();
        for segment in &row.segments {
            let style = segment_style(segment.role, params);
            painter.put_str(column, y + row.y, &segment.text, style);
            column += segment.width();
        }
    }

    if let Some(chars) = params.border {
        painter.border(
            x,
            y,
            layout.box_width,
            layout.box_height,
            chars,
            params.border_style,
        );
    }
}

/// The style for one role at the current zoom level.
fn segment_style(role: SegmentRole, params: &ZoomParams) -> ratatui::style::Style {
    match role {
        SegmentRole::Title => params.title_style,
        SegmentRole::Name => params.name_style,
        SegmentRole::Type => params.type_style,
        SegmentRole::Flag => params.flag_style,
        SegmentRole::Detail => params.detail_style,
        SegmentRole::Padding => ratatui::style::Style::new(),
    }
}

/// Table name with the `(view)` marker the UI uses to tell views apart.
///
/// The name is [`Table::display_name`]: bare while it is unambiguous,
/// schema-qualified when another table in the diagram shares it.
fn title_text(table: &Table, params: &ZoomParams) -> String {
    let name = params.fit(table.display_name());
    if table.is_view {
        format!("{name} (view)")
    } else {
        name
    }
}

/// Append `count` blank columns of alignment filler.
fn push_padded(segments: &mut Vec<Segment>, count: usize) {
    if count > 0 {
        segments.push(Segment::new(SegmentRole::Padding, " ".repeat(count)));
    }
}

/// Collapse a multi-line default expression so a row stays one line high.
fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        text.trim().to_string()
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::BorderChars;
    use crate::model::{Column, KeyKind};
    use crate::zoom::ZoomLevel;

    fn orders() -> Table {
        Table::new("orders")
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("customer_id", "integer").with_key(KeyKind::Foreign),
                Column::new("placed_at", "timestamp with time zone")
                    .nullable(true)
                    .with_default("now()"),
            ])
            .with_primary_key(["id"])
    }

    #[test]
    fn titles_come_first_and_columns_follow() {
        let layout = measure(&orders(), &ZoomLevel::Normal.params());
        assert_eq!(layout.rows[0].kind, RowKind::Title);
        assert_eq!(layout.rows[1].kind, RowKind::ColumnName { index: 0 });
        assert_eq!(layout.rows[3].kind, RowKind::ColumnName { index: 2 });
        assert_eq!(layout.column_row(2), Some(layout.rows[3].y));
        assert_eq!(layout.column_row(9), None);
    }

    #[test]
    fn box_size_accounts_for_border_and_padding() {
        let params = ZoomLevel::Normal.params();
        let layout = measure(&orders(), &params);
        assert_eq!(
            layout.box_width,
            layout.content_width + params.border_width() + 2 * params.padding_x as i32
        );
        assert_eq!(
            layout.box_height,
            layout.content_height + params.border_height() + 2 * params.padding_y as i32
        );
    }

    #[test]
    fn columns_are_hidden_at_the_farthest_level() {
        let layout = measure(&orders(), &ZoomLevel::Out2.params());
        assert_eq!(layout.rows.len(), 1);
        assert_eq!(layout.rows[0].kind, RowKind::Title);
        assert_eq!(layout.box_height, 1);
        // No column rows survive at this level, so the title is the whole box.
        assert_eq!(layout.content_height, 1);
    }

    #[test]
    fn closest_level_uses_two_lines_per_column() {
        let params = ZoomLevel::In1.params();
        let layout = measure(&orders(), &params);
        let name_row = layout
            .rows
            .iter()
            .position(|row| matches!(row.kind, RowKind::ColumnName { index: 0 }))
            .expect("a name line for the first column");
        let detail_row = layout
            .rows
            .iter()
            .position(|row| matches!(row.kind, RowKind::ColumnDetail { index: 0 }))
            .expect("a detail line for the first column");
        assert_eq!(detail_row, name_row + 1, "the detail follows its name line");
        assert_eq!(layout.rows[detail_row].y, layout.rows[name_row].y + 1);
        let detail = &layout.rows[detail_row].segments;
        assert!(detail.iter().any(|s| s.text == "integer"));
        assert!(detail.iter().any(|s| s.text == "NOT NULL"));
    }

    #[test]
    fn defaults_are_shown_per_column_only_when_present() {
        let layout = measure(&orders(), &ZoomLevel::In1.params());
        let defaults: Vec<String> = layout
            .rows
            .iter()
            .flat_map(|row| row.segments.iter())
            .filter(|s| s.role == SegmentRole::Detail)
            .map(|s| s.text.clone())
            .collect();
        assert!(defaults.contains(&"now()".to_string()));
        assert_eq!(defaults.iter().filter(|d| *d == "now()").count(), 1);
    }

    #[test]
    fn types_and_flags_line_up_across_rows() {
        let layout = measure(&orders(), &ZoomLevel::Normal.params());
        let starts: Vec<i32> = layout
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::ColumnName { .. }))
            .map(|row| {
                let mut column = 0;
                for segment in &row.segments {
                    if segment.role == SegmentRole::Type {
                        return column;
                    }
                    column += segment.width();
                }
                -1
            })
            .collect();
        assert_eq!(starts.len(), 3);
        assert!(starts.iter().all(|start| *start == starts[0]));
        assert_eq!(starts[0], layout.type_column);
    }

    #[test]
    fn long_types_are_elided_to_the_width_cap() {
        let params = ZoomParams {
            max_content_width: 6,
            ..ZoomLevel::Normal.params()
        };
        let layout = measure(&orders(), &params);
        let types: Vec<&str> = layout
            .rows
            .iter()
            .flat_map(|row| row.segments.iter())
            .filter(|s| s.role == SegmentRole::Type)
            .map(|s| s.text.as_str())
            .collect();
        assert!(types.iter().all(|t| t.chars().count() <= 6));
        assert!(types.iter().any(|t| t.ends_with('…')));
    }

    #[test]
    fn multiline_defaults_are_collapsed() {
        let table = Table::new("t").with_columns(vec![
            Column::new("c", "text").with_default("line one\n    line two")
        ]);
        let layout = measure(&table, &ZoomLevel::In1.params());
        let texts: Vec<String> = layout
            .rows
            .iter()
            .flat_map(|row| row.segments.iter())
            .map(|s| s.text.clone())
            .collect();
        assert!(texts.contains(&"line one line two".to_string()));
    }

    #[test]
    fn views_are_labelled() {
        let layout = measure(&Table::new("v").as_view(), &ZoomLevel::Normal.params());
        assert_eq!(layout.rows[0].segments[0].text, "v (view)");
    }

    #[test]
    fn drawing_puts_a_closed_border_around_the_content() {
        let params = ZoomLevel::Normal.params();
        let layout = measure(&orders(), &params);
        let mut canvas = crate::canvas::Canvas::new(60, 12);
        let camera = crate::canvas::Camera::new(0, 0);
        {
            let mut painter = Painter::new(&mut canvas, camera);
            draw(&mut painter, &layout, 0, 0, &params);
        }
        let lines = canvas.to_lines();
        let width = layout.box_width as usize;
        let corners = BorderChars::SINGLE;
        assert_eq!(lines[0].chars().next(), Some(corners.top_left));
        assert_eq!(lines[0].chars().nth(width - 1), Some(corners.top_right));
        assert_eq!(
            lines[layout.box_height as usize - 1].chars().next(),
            Some(corners.bottom_left)
        );
        assert_eq!(
            lines[layout.box_height as usize - 1].chars().nth(width - 1),
            Some(corners.bottom_right)
        );
    }

    #[test]
    fn drawing_at_an_offset_shifts_the_whole_box() {
        let params = ZoomLevel::Normal.params();
        let layout = measure(&orders(), &params);
        let mut canvas = crate::canvas::Canvas::new(60, 20);
        {
            let mut painter = Painter::new(&mut canvas, crate::canvas::Camera::new(0, 0));
            draw(&mut painter, &layout, 7, 3, &params);
        }
        let lines = canvas.to_lines();
        assert!(lines[0].trim().is_empty());
        assert_eq!(lines[3].chars().nth(7), Some(BorderChars::SINGLE.top_left));
    }

    #[test]
    fn drawing_clips_instead_of_panicking_at_the_canvas_edge() {
        let params = ZoomLevel::Normal.params();
        let layout = measure(&orders(), &params);
        let mut canvas = crate::canvas::Canvas::new(10, 4);
        {
            let mut painter = Painter::new(&mut canvas, crate::canvas::Camera::new(0, 0));
            draw(&mut painter, &layout, 8, 3, &params);
        }
        assert_eq!(canvas.width(), 10);
    }

    #[test]
    fn contains_bounds_the_box() {
        let layout = measure(&orders(), &ZoomLevel::Normal.params());
        assert!(layout.contains(0, 0));
        assert!(layout.contains(layout.box_width - 1, layout.box_height - 1));
        assert!(!layout.contains(layout.box_width, 0));
        assert!(!layout.contains(0, -1));
    }

    #[test]
    fn title_uses_the_display_name() {
        let params = ZoomLevel::Normal.params();
        assert_eq!(title_text(&orders(), &params), "orders");
        assert_eq!(
            title_text(&Table::new("active_users").as_view(), &params),
            "active_users (view)"
        );

        // Colliding bare names fall back to the schema-qualified display name.
        let mut schema = crate::model::Schema::new();
        schema.tables.push(Table::new("users").in_schema("public"));
        schema.tables.push(Table::new("users").in_schema("audit"));
        schema.finalize_keys();
        assert_eq!(title_text(&schema.tables[0], &params), "public.users");
    }
}
