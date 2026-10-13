//! Screen space canvas, world→screen camera, and the *only* place clipping happens.
//!
//! Tables live at integer world coordinates. The camera is an integer offset and
//! the screen position of a world point is simply `world - camera`. Drawing code
//! works in world coordinates through a [`Painter`]; bounds checking (and
//! therefore clipping of half-offscreen boxes) happens in exactly one place:
//! [`Canvas::put`] and the handful of primitives built on top of it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// A single character cell on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    /// A blank cell carrying `style` (so background fills work).
    pub const fn blank(style: Style) -> Self {
        Cell { ch: ' ', style }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::blank(Style::new())
    }
}

/// A fixed-size grid of cells in *screen* coordinates.
///
/// The canvas is always exactly the size of the render area, so it never needs to
/// grow: world coordinates are translated by the camera before they arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Canvas {
    /// Allocate a blank canvas of `width` × `height` cells.
    pub fn new(width: u16, height: u16) -> Self {
        Canvas {
            width,
            height,
            cells: vec![Cell::default(); width as usize * height as usize],
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// The canvas as a [`Rect`] anchored at the origin.
    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Resize to the given dimensions, discarding all content.
    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.cells = vec![Cell::default(); width as usize * height as usize];
    }

    /// Blank every cell.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        Some(y as usize * self.width as usize + x as usize)
    }

    /// Write one character. Returns `false` (and does nothing) when the position
    /// is off-canvas — this bounds check is the whole of the clipping logic.
    pub fn put(&mut self, x: i32, y: i32, ch: char, style: Style) -> bool {
        match self.index(x, y) {
            Some(index) => {
                self.cells[index] = Cell { ch, style };
                true
            }
            None => false,
        }
    }

    /// Read one cell, `None` when off-canvas.
    pub fn cell(&self, x: i32, y: i32) -> Option<&Cell> {
        self.index(x, y).map(|index| &self.cells[index])
    }

    /// Apply `style` to the cell at `(x, y)` keeping its character.
    pub fn set_style(&mut self, x: i32, y: i32, style: Style) -> bool {
        match self.index(x, y) {
            Some(index) => {
                self.cells[index].style = style;
                true
            }
            None => false,
        }
    }

    /// Fill a rectangle with `ch`, clipped to the canvas.
    pub fn fill(&mut self, x: i32, y: i32, width: i32, height: i32, ch: char, style: Style) {
        for dy in 0..height {
            for dx in 0..width {
                self.put(x + dx, y + dy, ch, style);
            }
        }
    }

    /// Apply `style` to every cell inside a rectangle, keeping characters. Used
    /// for the selection highlight.
    pub fn style_rect(&mut self, x: i32, y: i32, width: i32, height: i32, style: Style) {
        for dy in 0..height {
            for dx in 0..width {
                self.set_style(x + dx, y + dy, style);
            }
        }
    }

    /// Write a string; returns the number of columns advanced.
    pub fn put_str(&mut self, x: i32, y: i32, text: &str, style: Style) -> i32 {
        let mut cursor = x;
        for ch in text.chars() {
            self.put(cursor, y, ch, style);
            cursor += 1;
        }
        cursor - x
    }

    /// Copy the canvas into a ratatui buffer at `area`, clipped to both.
    pub fn blit(&self, buf: &mut Buffer, area: Rect) {
        let width = self.width.min(area.width);
        let height = self.height.min(area.height);
        for y in 0..height {
            for x in 0..width {
                let source = self.cells[y as usize * self.width as usize + x as usize];
                if let Some(target) = buf.cell_mut((area.x + x, area.y + y)) {
                    target.set_char(source.ch);
                    target.set_style(source.style);
                }
            }
        }
    }

    /// The canvas as plain strings, one per row (used by tests and by non-TUI
    /// debugging).
    pub fn to_lines(&self) -> Vec<String> {
        (0..self.height)
            .map(|y| {
                (0..self.width)
                    .map(|x| self.cells[y as usize * self.width as usize + x as usize].ch)
                    .collect()
            })
            .collect()
    }
}

/// Integer camera offset into the world.
///
/// `screen = world - camera`, so panning right (`dx > 0`) moves the viewport to
/// the right, which means world content appears to move left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Camera {
    pub x: i32,
    pub y: i32,
}

impl Camera {
    pub const fn new(x: i32, y: i32) -> Self {
        Camera { x, y }
    }

    /// Screen column of a world column.
    pub const fn screen_x(&self, world_x: i32) -> i32 {
        world_x - self.x
    }

    /// Screen row of a world row.
    pub const fn screen_y(&self, world_y: i32) -> i32 {
        world_y - self.y
    }

    /// World column of a screen column.
    pub const fn world_x(&self, screen_x: i32) -> i32 {
        screen_x + self.x
    }

    /// World row of a screen row.
    pub const fn world_y(&self, screen_y: i32) -> i32 {
        screen_y + self.y
    }

    /// Move the camera by a delta.
    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.x += dx;
        self.y += dy;
    }

    /// Put world point `(world_x, world_y)` in the middle of a `width` × `height`
    /// viewport.
    pub fn center_on(&mut self, world_x: i32, world_y: i32, width: u16, height: u16) {
        self.x = world_x - width as i32 / 2;
        self.y = world_y - height as i32 / 2;
    }

    /// Keep the viewport inside a world bounding box, allowing a small margin.
    ///
    /// `(x, y, width, height)` is the world bounding box; the viewport is clamped
    /// so it never scrolls past the content by more than `margin` cells.
    pub fn clamp_to_world(
        &mut self,
        world: (i32, i32, i32, i32),
        viewport_width: u16,
        viewport_height: u16,
        margin: i32,
    ) {
        let (wx, wy, ww, wh) = world;
        let min_x = wx - margin;
        let max_x = (wx + ww + margin - viewport_width as i32).max(min_x);
        let min_y = wy - margin;
        let max_y = (wy + wh + margin - viewport_height as i32).max(min_y);
        self.x = self.x.clamp(min_x, max_x);
        self.y = self.y.clamp(min_y, max_y);
    }
}

/// Box drawing characters for a table border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderChars {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

impl BorderChars {
    pub const SINGLE: BorderChars = BorderChars {
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
        horizontal: '─',
        vertical: '│',
    };

    pub const DOUBLE: BorderChars = BorderChars {
        top_left: '╔',
        top_right: '╗',
        bottom_left: '╚',
        bottom_right: '╝',
        horizontal: '═',
        vertical: '║',
    };
}

/// Draws in world coordinates, translating to screen space on the way to the
/// [`Canvas`]. Every method is clipping-safe: off-screen geometry is silently
/// dropped.
pub struct Painter<'a> {
    pub canvas: &'a mut Canvas,
    pub camera: Camera,
}

impl<'a> Painter<'a> {
    pub fn new(canvas: &'a mut Canvas, camera: Camera) -> Self {
        Painter { canvas, camera }
    }

    /// Put a character at a world position.
    pub fn put(&mut self, world_x: i32, world_y: i32, ch: char, style: Style) -> bool {
        let x = self.camera.screen_x(world_x);
        let y = self.camera.screen_y(world_y);
        self.canvas.put(x, y, ch, style)
    }

    /// Write a string at a world position; returns columns advanced.
    pub fn put_str(&mut self, world_x: i32, world_y: i32, text: &str, style: Style) -> i32 {
        let mut cursor = world_x;
        for ch in text.chars() {
            self.put(cursor, world_y, ch, style);
            cursor += 1;
        }
        cursor - world_x
    }

    /// Fill a world rectangle with `ch`.
    pub fn fill(&mut self, x: i32, y: i32, width: i32, height: i32, ch: char, style: Style) {
        for dy in 0..height {
            for dx in 0..width {
                self.put(x + dx, y + dy, ch, style);
            }
        }
    }

    /// Apply `style` to a world rectangle keeping its characters.
    pub fn style_rect(&mut self, x: i32, y: i32, width: i32, height: i32, style: Style) {
        for dy in 0..height {
            for dx in 0..width {
                let sx = self.camera.screen_x(x + dx);
                let sy = self.camera.screen_y(y + dy);
                self.canvas.set_style(sx, sy, style);
            }
        }
    }
    /// Transform the style of every cell in a world rectangle, keeping both the
    /// characters and the parts of the style the patch does not touch. This is how
    /// the selection highlight is applied without redrawing text.
    pub fn patch_style_rect<F>(&mut self, x: i32, y: i32, width: i32, height: i32, patch: F)
    where
        F: Fn(Style) -> Style,
    {
        for dy in 0..height {
            for dx in 0..width {
                let sx = self.camera.screen_x(x + dx);
                let sy = self.camera.screen_y(y + dy);
                if let Some(cell) = self.canvas.cell(sx, sy) {
                    let style = patch(cell.style);
                    self.canvas.set_style(sx, sy, style);
                }
            }
        }
    }

    /// Draw a rectangle border of `width` × `height` at world `(x, y)`.
    pub fn border(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        chars: BorderChars,
        style: Style,
    ) {
        if width < 2 || height < 2 {
            return;
        }
        self.put(x, y, chars.top_left, style);
        self.put(x + width - 1, y, chars.top_right, style);
        self.put(x, y + height - 1, chars.bottom_left, style);
        self.put(x + width - 1, y + height - 1, chars.bottom_right, style);
        for dx in 1..width - 1 {
            self.put(x + dx, y, chars.horizontal, style);
            self.put(x + dx, y + height - 1, chars.horizontal, style);
        }
        for dy in 1..height - 1 {
            self.put(x, y + dy, chars.vertical, style);
            self.put(x + width - 1, y + dy, chars.vertical, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn put_rejects_out_of_bounds_without_panicking() {
        let mut canvas = Canvas::new(4, 3);
        assert!(!canvas.put(-1, 0, 'x', Style::new()));
        assert!(!canvas.put(0, -1, 'x', Style::new()));
        assert!(!canvas.put(4, 0, 'x', Style::new()));
        assert!(!canvas.put(0, 3, 'x', Style::new()));
        assert!(!canvas.put(i32::MIN, i32::MAX, 'x', Style::new()));
    }

    #[test]
    fn put_accepts_corners() {
        let mut canvas = Canvas::new(4, 3);
        assert!(canvas.put(0, 0, 'a', Style::new()));
        assert!(canvas.put(3, 2, 'b', Style::new()));
        assert_eq!(canvas.cell(0, 0).unwrap().ch, 'a');
        assert_eq!(canvas.cell(3, 2).unwrap().ch, 'b');
    }

    #[test]
    fn fill_and_style_rect_clip_instead_of_growing() {
        let mut canvas = Canvas::new(4, 4);
        canvas.fill(-2, -2, 4, 4, '#', Style::new().fg(Color::Red));
        assert_eq!(canvas.cell(0, 0).unwrap().ch, '#');
        assert_eq!(canvas.cell(1, 1).unwrap().ch, '#');
        assert_eq!(canvas.cell(2, 2).unwrap().ch, ' ');
        canvas.style_rect(-1, -1, 3, 3, Style::new().bg(Color::Blue));
        assert_eq!(canvas.cell(0, 0).unwrap().style.bg, Some(Color::Blue));
    }

    #[test]
    fn world_to_screen_transform() {
        let camera = Camera::new(10, 5);
        assert_eq!(camera.screen_x(12), 2);
        assert_eq!(camera.screen_y(5), 0);
        assert_eq!(camera.screen_x(3), -7);
        assert_eq!(camera.world_x(2), 12);
        assert_eq!(camera.world_y(0), 5);
    }

    #[test]
    fn painter_clips_world_geometry() {
        // A 6x3 box starting at world (0,0), seen through a 4x3 canvas whose
        // camera sits at world (2,0): we observe columns 2..5 and rows 0..2,
        // so the left edge and the left corners are clipped away.
        let mut canvas = Canvas::new(4, 3);
        {
            let mut painter = Painter::new(&mut canvas, Camera::new(2, 0));
            painter.border(0, 0, 6, 3, BorderChars::SINGLE, Style::new());
        }
        let lines = canvas.to_lines();
        assert_eq!(lines[0], "───┐");
        assert_eq!(lines[1], "   │");
        assert_eq!(lines[2], "───┘");
    }

    #[test]
    fn blit_copies_into_buffer() {
        let mut canvas = Canvas::new(4, 2);
        canvas.put(0, 0, 'x', Style::new().fg(Color::Green));
        canvas.put(3, 1, 'y', Style::new());
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
        canvas.blit(&mut buf, Rect::new(2, 1, 6, 3));
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), "x");
        assert_eq!(buf.cell((2, 1)).unwrap().fg, Color::Green);
        assert_eq!(buf.cell((5, 2)).unwrap().symbol(), "y");
        // Outside the destination area nothing was written.
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn blit_clips_to_the_smaller_of_canvas_and_area() {
        let mut canvas = Canvas::new(10, 10);
        canvas.put(9, 9, 'z', Style::new());
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        canvas.blit(&mut buf, Rect::new(3, 0, 5, 5));
        // Only x = 0..4 of the canvas is inside the 5-wide area at x = 3, so the
        // cell at canvas (9, 9) is dropped rather than writing past the buffer.
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn center_on_puts_world_point_in_the_middle() {
        let mut camera = Camera::default();
        camera.center_on(100, 50, 80, 24);
        assert_eq!(camera.screen_x(100), 40);
        assert_eq!(camera.screen_y(50), 12);
    }

    #[test]
    fn clamp_keeps_viewport_over_content() {
        let mut camera = Camera::default();
        camera.clamp_to_world((0, 0, 100, 50), 80, 24, 2);
        // (0,0) is already inside the allowed window, so this is a no-op.
        assert_eq!((camera.x, camera.y), (0, 0));
        camera.pan(1000, 1000);
        camera.clamp_to_world((0, 0, 100, 50), 80, 24, 2);
        // 100x50 content in an 80x24 viewport with a 2 cell margin.
        assert_eq!((camera.x, camera.y), (22, 28));
    }
}
