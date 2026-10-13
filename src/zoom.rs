//! Zoom is level-of-detail plus spacing — never character scaling, which a
//! terminal cannot do.
//!
//! Four levels, from far away to close up:
//!
//! | level | shows | border |
//! |-------|-------|--------|
//! | `Out2` | table names only, tight packing | none |
//! | `Out1` | column names and key chips | single |
//! | `Normal` | column names, types, key chips | single |
//! | `In1` | every column on two lines with nullability and defaults, wide padding | double |
//!
//! Every level is described by a [`ZoomParams`] value, so the layout and drawing
//! code never matches on the enum.

use ratatui::style::{Color, Modifier, Style};

use crate::canvas::BorderChars;

/// The four levels of detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ZoomLevel {
    /// Far out: table names only, tight, no border.
    Out2,
    /// Mid: table names plus column names and key chips.
    Out1,
    /// Default: column names, types and key chips.
    #[default]
    Normal,
    /// Close up: full detail, one column per two lines, wide padding, double border.
    In1,
}

impl ZoomLevel {
    /// All levels from farthest to closest.
    pub const ALL: [ZoomLevel; 4] = [
        ZoomLevel::Out2,
        ZoomLevel::Out1,
        ZoomLevel::Normal,
        ZoomLevel::In1,
    ];

    /// Position in [`ZoomLevel::ALL`].
    pub const fn index(self) -> usize {
        match self {
            ZoomLevel::Out2 => 0,
            ZoomLevel::Out1 => 1,
            ZoomLevel::Normal => 2,
            ZoomLevel::In1 => 3,
        }
    }

    /// Level at `index`, saturating at both ends.
    pub const fn from_index(index: usize) -> Self {
        match index {
            0 => ZoomLevel::Out2,
            1 => ZoomLevel::Out1,
            2 => ZoomLevel::Normal,
            _ => ZoomLevel::In1,
        }
    }

    /// One level closer, saturating at [`ZoomLevel::In1`].
    pub const fn closer(self) -> Self {
        ZoomLevel::from_index(self.index() + 1)
    }

    /// One level farther, saturating at [`ZoomLevel::Out2`].
    pub const fn farther(self) -> Self {
        match self.index() {
            0 => ZoomLevel::Out2,
            index => ZoomLevel::from_index(index - 1),
        }
    }

    /// Short label for the status bar.
    pub const fn label(self) -> &'static str {
        match self {
            ZoomLevel::Out2 => "out2",
            ZoomLevel::Out1 => "out1",
            ZoomLevel::Normal => "normal",
            ZoomLevel::In1 => "in1",
        }
    }

    /// The rendering parameters for this level.
    pub const fn params(self) -> ZoomParams {
        match self {
            ZoomLevel::Out2 => ZoomParams {
                padding_x: 0,
                padding_y: 0,
                box_gap_x: 4,
                box_gap_y: 2,
                border: None,
                show_columns: false,
                show_types: false,
                show_flags: false,
                show_nullability: false,
                show_defaults: false,
                two_line_columns: false,
                max_content_width: 22,
                title_style: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                border_style: Style::new().fg(Color::DarkGray),
                name_style: Style::new(),
                type_style: Style::new(),
                flag_style: Style::new().fg(Color::DarkGray),
                detail_style: Style::new().fg(Color::DarkGray),
            },
            ZoomLevel::Out1 => ZoomParams {
                padding_x: 1,
                padding_y: 0,
                box_gap_x: 6,
                box_gap_y: 3,
                border: Some(BorderChars::SINGLE),
                show_columns: true,
                show_types: false,
                show_flags: true,
                show_nullability: false,
                show_defaults: false,
                two_line_columns: false,
                max_content_width: 28,
                title_style: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                border_style: Style::new().fg(Color::DarkGray),
                name_style: Style::new(),
                type_style: Style::new(),
                flag_style: Style::new().fg(Color::Yellow),
                detail_style: Style::new().fg(Color::DarkGray),
            },
            ZoomLevel::Normal => ZoomParams {
                padding_x: 1,
                padding_y: 0,
                box_gap_x: 8,
                box_gap_y: 4,
                border: Some(BorderChars::SINGLE),
                show_columns: true,
                show_types: true,
                show_flags: true,
                show_nullability: false,
                show_defaults: false,
                two_line_columns: false,
                max_content_width: 40,
                title_style: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                border_style: Style::new().fg(Color::DarkGray),
                name_style: Style::new(),
                type_style: Style::new().fg(Color::Cyan),
                flag_style: Style::new().fg(Color::Yellow),
                detail_style: Style::new().fg(Color::DarkGray),
            },
            ZoomLevel::In1 => ZoomParams {
                padding_x: 2,
                padding_y: 1,
                box_gap_x: 10,
                box_gap_y: 5,
                border: Some(BorderChars::DOUBLE),
                show_columns: true,
                show_types: true,
                show_flags: true,
                show_nullability: true,
                show_defaults: true,
                two_line_columns: true,
                max_content_width: 60,
                title_style: Style::new()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
                border_style: Style::new().fg(Color::Gray),
                name_style: Style::new().add_modifier(Modifier::BOLD),
                type_style: Style::new().fg(Color::Cyan),
                flag_style: Style::new().fg(Color::Yellow),
                detail_style: Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            },
        }
    }
}

/// Everything the layout and drawing code needs to know about detail level.
///
/// Kept as one copyable value so it can be threaded through layout, measurement
/// and drawing without any of them matching on [`ZoomLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomParams {
    /// Blank columns between the border and the content.
    pub padding_x: u16,
    /// Blank lines between the border and the content.
    pub padding_y: u16,
    /// Blank columns between neighbouring boxes.
    pub box_gap_x: u16,
    /// Blank lines between rows of boxes.
    pub box_gap_y: u16,
    /// Border characters, or `None` for a borderless box.
    pub border: Option<BorderChars>,
    /// Draw the column rows at all.
    pub show_columns: bool,
    /// Append the column type.
    pub show_types: bool,
    /// Append the `PK` / `FK` / `PK,FK` chip.
    pub show_flags: bool,
    /// Append `NULL` / `NOT NULL` (second line).
    pub show_nullability: bool,
    /// Append `= <default>` (second line).
    pub show_defaults: bool,
    /// Render each column on two lines instead of one.
    pub two_line_columns: bool,
    /// Soft cap on content width before the type is elided.
    pub max_content_width: usize,
    pub title_style: Style,
    pub border_style: Style,
    pub name_style: Style,
    pub type_style: Style,
    pub flag_style: Style,
    pub detail_style: Style,
}

impl ZoomParams {
    /// Number of lines one column occupies.
    pub const fn rows_per_column(&self) -> i32 {
        if self.two_line_columns {
            2
        } else {
            1
        }
    }

    /// Whether a border is drawn.
    pub const fn has_border(&self) -> bool {
        self.border.is_some()
    }

    /// Columns taken by the border (0 or 2).
    pub const fn border_width(&self) -> i32 {
        if self.has_border() {
            2
        } else {
            0
        }
    }

    /// Lines taken by the border (0 or 2).
    pub const fn border_height(&self) -> i32 {
        if self.has_border() {
            2
        } else {
            0
        }
    }

    /// Horizontal offset of the content from the box's left edge.
    pub const fn inner_x(&self) -> i32 {
        (if self.has_border() { 1 } else { 0 }) + self.padding_x as i32
    }

    /// Vertical offset of the content from the box's top edge.
    pub const fn inner_y(&self) -> i32 {
        (if self.has_border() { 1 } else { 0 }) + self.padding_y as i32
    }

    /// Elide `text` to at most [`ZoomParams::max_content_width`] columns.
    pub fn fit(&self, text: &str) -> String {
        if text.chars().count() <= self.max_content_width {
            return text.to_string();
        }
        let keep = self.max_content_width.saturating_sub(1);
        let mut out: String = text.chars().take(keep).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_step_and_saturate() {
        assert_eq!(ZoomLevel::Out2.farther(), ZoomLevel::Out2);
        assert_eq!(ZoomLevel::Out2.closer(), ZoomLevel::Out1);
        assert_eq!(ZoomLevel::In1.closer(), ZoomLevel::In1);
        assert_eq!(ZoomLevel::In1.farther(), ZoomLevel::Normal);
        assert_eq!(ZoomLevel::default(), ZoomLevel::Normal);
    }

    #[test]
    fn index_round_trips() {
        for level in ZoomLevel::ALL {
            assert_eq!(ZoomLevel::from_index(level.index()), level);
        }
    }

    #[test]
    fn detail_only_ever_increases_with_zoom() {
        let params: Vec<ZoomParams> = ZoomLevel::ALL.iter().map(|l| l.params()).collect();
        for pair in params.windows(2) {
            let (far, near) = (pair[0], pair[1]);
            assert!(near.padding_x >= far.padding_x);
            assert!(near.padding_y >= far.padding_y);
            assert!(near.max_content_width >= far.max_content_width);
        }
    }

    #[test]
    fn out2_hides_columns_and_border() {
        let params = ZoomLevel::Out2.params();
        assert!(!params.has_border());
        assert!(!params.show_columns);
        assert_eq!(params.inner_x(), 0);
    }

    #[test]
    fn in1_shows_two_line_columns() {
        let params = ZoomLevel::In1.params();
        assert_eq!(params.rows_per_column(), 2);
        assert!(params.show_defaults && params.show_nullability);
        assert_eq!(params.border, Some(BorderChars::DOUBLE));
    }

    #[test]
    fn fit_elides_long_text() {
        let params = ZoomParams {
            max_content_width: 5,
            ..ZoomLevel::Normal.params()
        };
        assert_eq!(params.fit("abc"), "abc");
        assert_eq!(params.fit("abcdefgh"), "abcd…");
    }
}
