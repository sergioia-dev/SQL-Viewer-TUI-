//! The interactive application: command line, state, input handling and drawing.
//!
//! The app only ever sees the frozen [`Schema`]; which database it came from is
//! reduced to a label in the status bar. The renderer keeps a strict world /
//! screen split: every coordinate handed to [`Painter`] is a world coordinate,
//! and the [`Camera`] is the only thing that turns one into a screen cell.

use std::fmt;
use std::io;
use std::io::IsTerminal;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::canvas::{Camera, Canvas, Painter};
use crate::edges::{self, EdgeRoute, RouteOptions};
use crate::introspect::{self, DriverKind, IntrospectError, Target};
use crate::layout::{layout_grid, DiagramLayout};
use crate::model::Schema;
use crate::table_view;
use crate::zoom::ZoomLevel;

/// Help text printed by `--help`.
pub const USAGE: &str = "\
sqlv — a keyboard-driven terminal ER diagram viewer

USAGE:
    sqlv [OPTIONS] [URL]

OPTIONS:
    --demo             Draw the built-in fixture schema; no database needed
    --url <URL>        Connection string (positional URLs are accepted too)
    --db <DRIVER>      Force the driver when the URL scheme is ambiguous:
                       sqlite, postgres, mysql, mariadb, mssql
    -h, --help         Print this help
    -V, --version      Print the version

KEYS (inside the viewer):
    h j k l / arrows   pan          + / -    zoom in / out
    g / G              top / bottom Tab      next table
    Ctrl+d / Ctrl+u    page         /        search tables
    ?                  help         q        quit

EXAMPLES:
    sqlv --demo
    sqlv sqlite:///data/app.db
    sqlv postgres://user:secret@localhost/app
    sqlv --db mssql 'mssql://sa:secret@localhost:1433/app'
";

/// Parsed command line arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Draw the built-in demo schema instead of opening a database.
    pub demo: bool,
    /// The connection string, when one was given.
    pub url: Option<String>,
    /// A driver forced with `--db`, for URLs whose scheme is ambiguous.
    pub db: Option<String>,
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Start the viewer.
    Run(Options),
    /// Print [`USAGE`] and exit successfully.
    Help,
    /// Print the version and exit successfully.
    Version,
}

impl Options {
    /// Parse the arguments that follow the program name.
    ///
    /// Kept deliberately small: `clap` is not available here, and the whole
    /// surface is a handful of flags. A bare word is treated as the URL.
    pub fn parse<I, S>(args: I) -> Result<Invocation, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut options = Options::default();
        let mut args = args.into_iter().map(Into::into);
        let mut positional_only = false;

        while let Some(arg) = args.next() {
            if positional_only {
                options.set_url(&arg)?;
                continue;
            }
            match arg.as_str() {
                "--" => positional_only = true,
                "-h" | "--help" => return Ok(Invocation::Help),
                "-V" | "--version" => return Ok(Invocation::Version),
                "--demo" => options.demo = true,
                "--url" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--url needs a connection string".to_string())?;
                    options.set_url(&value)?;
                }
                "--db" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--db needs a driver name".to_string())?;
                    options.set_db(&value)?;
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--url=") {
                        options.set_url(value)?;
                    } else if let Some(value) = arg.strip_prefix("--db=") {
                        options.set_db(value)?;
                    } else if arg.starts_with('-') {
                        return Err(format!("unknown option {arg:?} (try --help)"));
                    } else {
                        options.set_url(&arg)?;
                    }
                }
            }
        }

        if !options.demo && options.url.is_none() {
            return Err(
                "no database given: pass a connection string or --demo (try --help)".to_string(),
            );
        }
        Ok(Invocation::Run(options))
    }

    fn set_url(&mut self, url: &str) -> Result<(), String> {
        if self.url.is_some() {
            return Err("more than one connection string given".to_string());
        }
        if url.is_empty() {
            return Err("the connection string is empty".to_string());
        }
        self.url = Some(url.to_string());
        Ok(())
    }

    fn set_db(&mut self, driver: &str) -> Result<(), String> {
        if self.db.is_some() {
            return Err("--db given more than once".to_string());
        }
        if DriverKind::parse(driver).is_none() {
            return Err(format!(
                "unknown driver {driver:?}: expected sqlite, postgres, mysql, mariadb or mssql"
            ));
        }
        self.db = Some(driver.to_string());
        Ok(())
    }
}

/// Anything that can stop the viewer from running.
#[derive(Debug)]
pub enum AppError {
    /// The schema could not be read.
    Introspect(IntrospectError),
    /// The terminal could not be set up or torn down.
    Io(io::Error),
    /// The viewer was started with nothing to draw in.
    NotATerminal,
}

impl From<IntrospectError> for AppError {
    fn from(error: IntrospectError) -> Self {
        AppError::Introspect(error)
    }
}

impl From<io::Error> for AppError {
    fn from(error: io::Error) -> Self {
        AppError::Io(error)
    }
}

impl AppError {
    /// A suggestion to print under the error, when we have one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            AppError::Introspect(error) => error.hint(),
            AppError::NotATerminal => {
                Some("Run sqlv from an interactive terminal; --help and --version work anywhere.")
            }
            AppError::Io(_) => None,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Introspect(error) => write!(f, "{error}"),
            AppError::Io(error) => write!(f, "terminal error: {error}"),
            AppError::NotATerminal => {
                write!(
                    f,
                    "standard output is not a terminal, so there is nothing to draw in"
                )
            }
        }
    }
}

impl std::error::Error for AppError {}

/// Load a schema and run the viewer until the user quits.
pub fn run(options: Options) -> Result<(), AppError> {
    if options.demo {
        let schema = introspect::demo::demo_schema();
        return with_terminal(&schema, "demo fixture".to_string());
    }

    let url = options.url.clone().unwrap_or_default();
    let target = Target::resolve(&url, options.db.as_deref())?;
    let introspector = introspect::open(&target)?;
    let source = introspector.describe();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(AppError::Io)?;

    // With nothing to draw in the query is still attempted, so that a bad
    // connection string reports its real problem instead of a terminal error.
    if !io::stdout().is_terminal() {
        runtime.block_on(introspector.load())?;
        return Err(AppError::NotATerminal);
    }

    // The terminal is claimed before the query so the user sees progress; every
    // early return from here on has to hand it back.
    let mut terminal = ratatui::init();
    if let Err(error) = draw_loading(&mut terminal, &source) {
        ratatui::restore();
        return Err(error.into());
    }

    let schema = match runtime.block_on(introspector.load()) {
        Ok(schema) => schema,
        Err(error) => {
            ratatui::restore();
            return Err(error.into());
        }
    };
    drop(runtime);

    let result = event_loop(&mut terminal, &schema, &source);
    ratatui::restore();
    result
}

/// Claim the terminal, refusing cleanly when there is none to draw in.
fn claim_terminal() -> Result<DefaultTerminal, AppError> {
    if !io::stdout().is_terminal() {
        return Err(AppError::NotATerminal);
    }
    Ok(ratatui::init())
}

/// Claim the terminal, run the viewer, and always hand the terminal back.
fn with_terminal(schema: &Schema, source: String) -> Result<(), AppError> {
    let mut terminal = claim_terminal()?;
    let result = event_loop(&mut terminal, schema, &source);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    schema: &Schema,
    source: &str,
) -> Result<(), AppError> {
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let mut app = App::new(schema, source.to_string(), area);

    loop {
        terminal.draw(|frame| app.draw(frame))?;
        match event::read()? {
            Event::Key(key) => {
                // Windows delivers release events too; only act on presses.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.handle_key(key) {
                    return Ok(());
                }
            }
            Event::Resize(width, height) => app.resize(Rect::new(0, 0, width, height)),
            _ => {}
        }
    }
}

/// The first frame, drawn while the schema is being read.
fn draw_loading(terminal: &mut DefaultTerminal, source: &str) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let panel = centered(area, 54, 6);
        let text = vec![
            Line::from(Span::styled(
                "Loading schema…",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                source.to_string(),
                Style::new().fg(Color::DarkGray),
            )),
        ];
        let block = Block::bordered()
            .title(" sqlv ")
            .border_style(Style::new().fg(Color::DarkGray));
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            panel,
        );
    })?;
    Ok(())
}

/// A rectangle of at most `width` × `height` centred inside `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// An in-progress table search.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Search {
    query: String,
}

/// Keys shown in the `?` overlay.
const KEY_HELP: &[(&str, &str)] = &[
    ("h j k l  /  ← ↓ ↑ →", "pan the diagram"),
    ("+ / -", "zoom in / out"),
    ("g / G", "jump to the top / bottom"),
    ("Ctrl+d / Ctrl+u", "page down / up"),
    ("Tab / Shift+Tab", "select the next / previous table"),
    ("/", "search tables by name"),
    ("?", "close this help"),
    ("q / Esc", "quit"),
];

/// Column width reserved for the key column of the `?` overlay.
const KEY_HELP_KEY_WIDTH: usize = 22;

/// The whole viewer state.
pub struct App {
    schema: Schema,
    source: String,
    zoom: ZoomLevel,
    layout: DiagramLayout,
    routes: Vec<EdgeRoute>,
    camera: Camera,
    viewport: Rect,
    selected: Option<usize>,
    search: Option<Search>,
    help: bool,
    status: Option<String>,
}

impl App {
    /// Build the initial state: lay the schema out and start the camera at the
    /// top-left corner of the diagram.
    pub fn new(schema: &Schema, source: String, viewport: Rect) -> Self {
        let zoom = ZoomLevel::Normal;
        let layout = build_layout(schema, zoom, viewport.width);
        let routes = edges::route_edges(schema, &layout, RouteOptions::default());
        // Start at the top-left of the diagram: centring on the layout
        // centroid over-scrolls the first table, hiding its title row.
        let (bounds_x, bounds_y, _, _) = layout.bounds;
        let camera = Camera::new(bounds_x, bounds_y);

        let mut app = App {
            schema: schema.clone(),
            source,
            zoom,
            layout,
            routes,
            camera,
            viewport,
            selected: if schema.tables.is_empty() {
                None
            } else {
                Some(0)
            },
            search: None,
            help: false,
            status: None,
        };
        app.clamp_camera();
        app
    }

    /// The drawing area, less the status bar row.
    fn diagram_area(&self) -> Rect {
        diagram_area(self.viewport)
    }

    /// Handle one key press; returns `true` when the viewer should quit.
    ///
    /// The modal states come first: help swallows everything but `?`, `Esc` and
    /// `q`, and the search prompt swallows everything so `q` can be typed.
    ///
    /// This is the whole input path, so a headless driver (a test or a tool) can
    /// replay a key sequence against [`App::draw`] without owning a terminal.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.help {
            match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Esc | KeyCode::Char('?') => self.help = false,
                _ => self.help = false,
            }
            return false;
        }

        if self.search.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.search = None,
                KeyCode::Backspace => {
                    if let Some(search) = self.search.as_mut() {
                        search.query.pop();
                    }
                    self.refresh_search();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(search) = self.search.as_mut() {
                        search.query.push(c);
                    }
                    self.refresh_search();
                }
                KeyCode::Tab | KeyCode::Down => self.select_step(1),
                KeyCode::BackTab | KeyCode::Up => self.select_step(-1),
                _ => {}
            }
            return false;
        }

        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => return true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_by(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.zoom_by(-1),
            KeyCode::Char('/') => {
                self.search = Some(Search {
                    query: String::new(),
                })
            }
            KeyCode::Tab => self.select_step(1),
            KeyCode::BackTab => self.select_step(-1),
            KeyCode::Char('g') | KeyCode::Home => self.jump_to_top(),
            KeyCode::Char('G') | KeyCode::End => self.jump_to_bottom(),
            KeyCode::Char('d') if control => self.page(1),
            KeyCode::Char('u') if control => self.page(-1),
            KeyCode::Char('h') | KeyCode::Left => self.pan(-1, 0),
            KeyCode::Char('l') | KeyCode::Right => self.pan(1, 0),
            KeyCode::Char('k') | KeyCode::Up => self.pan(0, -1),
            KeyCode::Char('j') | KeyCode::Down => self.pan(0, 1),
            KeyCode::PageDown => self.page(1),
            KeyCode::PageUp => self.page(-1),
            _ => {}
        }
        false
    }

    /// The viewport changed; re-measure and keep the camera legal.
    pub fn resize(&mut self, viewport: Rect) {
        self.viewport = viewport;
        self.relayout();
    }

    fn pan(&mut self, dx: i32, dy: i32) {
        self.camera.pan(dx, dy);
        self.clamp_camera();
    }

    fn page(&mut self, direction: i32) {
        let area = self.diagram_area();
        let step = (area.height as i32 / 2).max(1);
        self.pan(0, direction * step);
    }

    fn jump_to_top(&mut self) {
        self.camera.y = self.layout.bounds.1;
        self.clamp_camera();
    }

    fn jump_to_bottom(&mut self) {
        self.camera.y = self.layout.bounds.1 + self.layout.bounds.3;
        self.clamp_camera();
    }

    fn zoom_by(&mut self, direction: i32) {
        let zoom = if direction > 0 {
            self.zoom.closer()
        } else {
            self.zoom.farther()
        };
        self.set_zoom(zoom);
    }

    /// Switch detail levels while keeping the world point under the middle of
    /// the viewport exactly where it was.
    fn set_zoom(&mut self, zoom: ZoomLevel) {
        if zoom == self.zoom {
            return;
        }
        let area = self.diagram_area();
        let anchor_x = self.camera.world_x(area.width as i32 / 2);
        let anchor_y = self.camera.world_y(area.height as i32 / 2);

        self.zoom = zoom;
        self.layout = build_layout(&self.schema, zoom, area.width);
        self.routes = edges::route_edges(&self.schema, &self.layout, RouteOptions::default());
        self.camera
            .center_on(anchor_x, anchor_y, area.width, area.height);
        self.clamp_camera();
        self.set_status(format!("zoom {} · re-laid out", zoom.label()));
    }

    /// Re-measure the layout for the current zoom and viewport width.
    fn relayout(&mut self) {
        let area = self.diagram_area();
        self.layout = build_layout(&self.schema, self.zoom, area.width);
        self.routes = edges::route_edges(&self.schema, &self.layout, RouteOptions::default());
        self.clamp_camera();
    }

    fn clamp_camera(&mut self) {
        let area = self.diagram_area();
        self.camera
            .clamp_to_world(self.layout.bounds, area.width, area.height, 2);
    }

    fn select_step(&mut self, direction: i64) {
        let count = self.schema.tables.len();
        if count == 0 {
            return;
        }
        let index = match self.selected {
            Some(current) => (current as i64 + direction).rem_euclid(count as i64) as usize,
            None if direction >= 0 => 0,
            None => count - 1,
        };
        self.select(index);
    }

    fn select(&mut self, index: usize) {
        if index >= self.schema.tables.len() {
            return;
        }
        self.selected = Some(index);
        self.ensure_visible(index);
    }

    /// Centre the camera on a table unless its whole box is already visible.
    fn ensure_visible(&mut self, index: usize) {
        let area = self.diagram_area();
        let Some(placement) = self.layout.placement(index) else {
            return;
        };
        let (x, y, width, height) = placement.rect();
        let visible = x >= self.camera.x
            && y >= self.camera.y
            && x + width <= self.camera.x + area.width as i32
            && y + height <= self.camera.y + area.height as i32;
        if visible {
            return;
        }
        let (center_x, center_y) = placement.center();
        self.camera
            .center_on(center_x, center_y, area.width, area.height);
        self.clamp_camera();
    }

    /// Re-run the live search and jump to the first match.
    fn refresh_search(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let needle = search.query.to_ascii_lowercase();
        if needle.is_empty() {
            return;
        }
        let found = self
            .schema
            .tables
            .iter()
            .position(|table| table.name.to_ascii_lowercase().contains(&needle));
        if let Some(index) = found {
            self.select(index);
        }
    }

    fn set_status(&mut self, message: String) {
        self.status = Some(message);
    }

    /// Number of tables matching the current search.
    fn search_matches(&self) -> usize {
        let Some(search) = self.search.as_ref() else {
            return 0;
        };
        let needle = search.query.to_ascii_lowercase();
        if needle.is_empty() {
            return 0;
        }
        self.schema
            .tables
            .iter()
            .filter(|table| table.name.to_ascii_lowercase().contains(&needle))
            .count()
    }

    /// The selected table's label, when there is one.
    ///
    /// Uses [`Table::display_name`], the same name the diagram box shows, so
    /// the status bar and the box never disagree.
    fn selected_label(&self) -> Option<String> {
        let index = self.selected?;
        self.schema.tables.get(index).map(|table| {
            let name = table.display_name();
            if table.is_view {
                format!("{name} (view)")
            } else {
                name.to_string()
            }
        })
    }

    /// Draw one frame: diagram into a [`Canvas`], then status bar and overlays.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let diagram = diagram_area(area);
        let params = self.zoom.params();
        let mut canvas = Canvas::new(diagram.width, diagram.height);

        {
            let mut painter = Painter::new(&mut canvas, self.camera);
            for placement in &self.layout.placements {
                table_view::draw(
                    &mut painter,
                    &placement.layout,
                    placement.x,
                    placement.y,
                    &params,
                );
            }
            edges::draw_routes(
                &mut painter,
                &self.layout,
                &self.routes,
                &edges::EDGE_PALETTE,
            );
        }

        if let Some(index) = self.selected {
            if let Some(placement) = self.layout.placement(index) {
                let (x, y, width, height) = placement.rect();
                let mut painter = Painter::new(&mut canvas, self.camera);
                painter.patch_style_rect(x, y, width, height, |style| {
                    style.add_modifier(Modifier::REVERSED)
                });
            }
        }

        canvas.blit(frame.buffer_mut(), diagram);
        self.draw_status(frame, area);
        if self.help {
            self.draw_help(frame, area);
        }
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let bar = Rect::new(area.x, area.y + area.height - 1, area.width, 1);

        let line = if let Some(search) = self.search.as_ref() {
            Line::from(vec![
                Span::styled("/", Style::new().fg(Color::Yellow)),
                Span::styled(
                    search.query.clone(),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "   {} match(es)   Enter accept · Esc cancel",
                        self.search_matches()
                    ),
                    Style::new().fg(Color::DarkGray),
                ),
            ])
        } else {
            let selected = self.selected_label().unwrap_or_else(|| "—".to_string());
            let status = self
                .status
                .clone()
                .unwrap_or_else(|| format!("zoom {}", self.zoom.label()));
            Line::from(vec![
                Span::styled(self.source.clone(), Style::new().fg(Color::Cyan)),
                Span::styled(" · ", Style::new().fg(Color::DarkGray)),
                Span::styled(status, Style::new()),
                Span::styled(" · ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{} tables, {} columns",
                        self.schema.table_count(),
                        self.schema.column_count()
                    ),
                    Style::new(),
                ),
                Span::styled(" · ", Style::new().fg(Color::DarkGray)),
                Span::styled(selected, Style::new().fg(Color::Yellow)),
                Span::styled(" · ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    format!("camera {},{}", self.camera.x, self.camera.y),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(" · ", Style::new().fg(Color::DarkGray)),
                Span::styled("? help · q quit", Style::new().fg(Color::DarkGray)),
            ])
        };

        frame.render_widget(Paragraph::new(line), bar);
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let height = KEY_HELP.len() as u16 + 2;
        // Derive the width from the rows instead of hardcoding it, so the
        // longest binding is never clipped by the panel border.
        let width = KEY_HELP
            .iter()
            .map(|(_, description)| KEY_HELP_KEY_WIDTH + description.chars().count())
            .max()
            .unwrap_or(0) as u16
            + 2;
        let panel = centered(area, width, height);
        let lines: Vec<Line<'_>> = KEY_HELP
            .iter()
            .map(|(keys, description)| {
                Line::from(vec![
                    Span::styled(
                        format!("{keys:<KEY_HELP_KEY_WIDTH$}"),
                        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(*description, Style::new()),
                ])
            })
            .collect();
        let block = Block::bordered()
            .title(" keys ")
            .border_style(Style::new().fg(Color::DarkGray));
        frame.render_widget(Clear, panel);
        frame.render_widget(Paragraph::new(lines).block(block), panel);
    }
}

/// The diagram area for a terminal area of `area`.
fn diagram_area(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1))
}

/// Measure and place every table of `schema`.
fn build_layout(schema: &Schema, zoom: ZoomLevel, width: u16) -> DiagramLayout {
    layout_grid(schema, &zoom.params(), width.max(1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> Result<Invocation, String> {
        Options::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn demo_needs_no_url() {
        let invocation = run_args(&["--demo"]).unwrap();
        assert_eq!(
            invocation,
            Invocation::Run(Options {
                demo: true,
                url: None,
                db: None,
            })
        );
    }

    #[test]
    fn bare_argument_is_the_url() {
        let invocation = run_args(&["sqlite:///data/app.db"]).unwrap();
        match invocation {
            Invocation::Run(options) => {
                assert_eq!(options.url.as_deref(), Some("sqlite:///data/app.db"));
                assert!(!options.demo);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn url_flag_accepts_both_spellings() {
        for args in [
            vec!["--url", "postgres://localhost/app"],
            vec!["--url=postgres://localhost/app"],
        ] {
            let Invocation::Run(options) = run_args(&args).unwrap() else {
                panic!("expected Run");
            };
            assert_eq!(options.url.as_deref(), Some("postgres://localhost/app"));
        }
    }

    #[test]
    fn db_override_is_validated() {
        let invocation = run_args(&["--db", "mariadb", "mysql://localhost/app"]).unwrap();
        let Invocation::Run(options) = invocation else {
            panic!("expected Run");
        };
        assert_eq!(options.db.as_deref(), Some("mariadb"));

        let error = run_args(&["--db", "oracle", "foo://x"]).unwrap_err();
        assert!(error.contains("unknown driver"), "{error}");
    }

    #[test]
    fn missing_database_is_an_error() {
        let error = run_args(&[]).unwrap_err();
        assert!(error.contains("no database given"), "{error}");
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(run_args(&["-h"]).unwrap(), Invocation::Help);
        assert_eq!(run_args(&["--help"]).unwrap(), Invocation::Help);
        assert_eq!(run_args(&["-V"]).unwrap(), Invocation::Version);
        assert_eq!(run_args(&["--version"]).unwrap(), Invocation::Version);
    }

    #[test]
    fn unknown_option_is_rejected() {
        let error = run_args(&["--frobnicate"]).unwrap_err();
        assert!(error.contains("unknown option"), "{error}");
    }

    #[test]
    fn two_urls_are_rejected() {
        let error = run_args(&["sqlite:///a.db", "sqlite:///b.db"]).unwrap_err();
        assert!(error.contains("more than one"), "{error}");
    }

    #[test]
    fn double_dash_treats_everything_as_a_url() {
        // After `--` every argument is positional, so `--demo` becomes the URL
        // instead of the demo flag.
        let invocation = run_args(&["--", "--demo"]).expect("a positional URL");
        assert_eq!(
            invocation,
            Invocation::Run(Options {
                demo: false,
                url: Some("--demo".to_string()),
                db: None,
            })
        );
    }
}
