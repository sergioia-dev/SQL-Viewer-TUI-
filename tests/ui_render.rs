//! Headless UI tests: drive the real viewer and inspect the frame it would put
//! on the terminal, with no TTY, no database and no timing involved.
//!
//! [`App::draw`] only needs a `Frame` and [`App::handle_key`] is the entire
//! input path, so a [`TestBackend`] is enough to assert what a user sees and
//! what a key press does to it. That keeps the UI tests as fast and as
//! deterministic as the pure layout/edges tests beside them, and CI never needs
//! a PTY.
//!
//! Two layers are covered on purpose:
//!
//! * the *frame* — the text and styles the terminal is asked to show, and
//! * the *census* ([`demo_frame_glyph_census_is_stable`]) — how many of each
//!   box-drawing glyph a fixed-size frame contains.
//!
//! The census is also the cross-profile canary. A rustc 1.99.0 codegen bug used
//! to miscompile `EdgeRoute::path_chars` at `opt-level >= 2`, so the release
//! build could route connectors differently from the debug build (see
//! `notes/` and the `[profile.release]` comment in `Cargo.toml`). If this file
//! ever fails in one profile only, suspect a miscompile before suspecting the
//! drawing code.

use std::collections::BTreeSet;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::Terminal;

use sqlv::app::App;
use sqlv::introspect::demo::demo_schema;

/// The size the frame assertions below are written against.
const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

/// Every table the built-in `--demo` fixture defines.
const DEMO_TABLES: [&str; 9] = [
    "users",
    "categories",
    "products",
    "orders",
    "order_lines",
    "shipments",
    "tags",
    "product_tags",
    "active_users",
];

/// The box-drawing glyphs the viewer draws, in census order.
// Junction glyphs (`├ ┤ ┬ ┴`) are part of the census because they are easy to
// get wrong: a prong that points into its table still looks like a connector,
// and a swapped prong is invisible unless the census counts it.
const BOX_GLYPHS: [char; 10] = ['─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴'];
const JUNCTIONS: [char; 4] = ['├', '┤', '┬', '┴'];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn app() -> App {
    app_in(diagram_rect(WIDTH, HEIGHT))
}

/// An app attached to the given viewport, so the layout and the buffer agree.
fn app_in(viewport: Rect) -> App {
    App::new(
        &demo_schema(),
        "demo schema (built in)".to_string(),
        viewport,
    )
}

fn diagram_rect(width: u16, height: u16) -> Rect {
    Rect::new(0, 0, width, height)
}

/// Render one frame and hand back the buffer the backend received.
fn buffer_of(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("in-memory terminal backend");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("drawing a frame must not fail on an in-memory backend");
    terminal.backend().buffer().clone()
}

/// The frame as text: one line per terminal row, trailing blanks trimmed.
fn text_of(app: &App) -> String {
    text(&buffer_of(app, WIDTH, HEIGHT))
}

fn text(buffer: &Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            let line: String = (0..area.width)
                .map(|x| {
                    // Wide graphemes are absent from this UI, so the first char
                    // is the whole cell.
                    buffer
                        .cell((area.x + x, area.y + y))
                        .and_then(|cell| cell.symbol().chars().next())
                        .unwrap_or(' ')
                })
                .collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keeps a test from having to know where a cell lives.
fn contains(app: &App, needle: &str) -> bool {
    text_of(app).contains(needle)
}

/// The last frame row is the status bar.
fn status(app: &App) -> String {
    text_of(app)
        .lines()
        .last()
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// Reverse-video cells — how the viewer marks the selected table.
fn reversed_cells(app: &App) -> BTreeSet<(u16, u16)> {
    let buffer = buffer_of(app, WIDTH, HEIGHT);
    let area = buffer.area();
    let mut cells = BTreeSet::new();
    for y in 0..area.height {
        for x in 0..area.width {
            let position = (area.x + x, area.y + y);
            if let Some(cell) = buffer.cell(position) {
                if cell.modifier.contains(Modifier::REVERSED) {
                    cells.insert(position);
                }
            }
        }
    }
    cells
}

fn press(app: &mut App, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn press_shift(app: &mut App, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        press(app, KeyCode::Char(character));
    }
}

fn census(text: &str) -> String {
    BOX_GLYPHS
        .iter()
        .map(|glyph| format!("{glyph}={}", text.matches(*glyph).count()))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// What the demo schema draws
// ---------------------------------------------------------------------------

#[test]
fn demo_schema_draws_every_table_and_marks_the_view() {
    // A backend big enough for the whole diagram: this test is about content,
    // not about what happens to fit on a 100x30 terminal.
    let frame = text_of_in(&app_in(diagram_rect(200, 60)), 200, 60);
    for table in DEMO_TABLES {
        assert!(
            frame.contains(table),
            "table `{table}` is missing from the frame:\n{frame}"
        );
    }
    assert!(
        frame.contains("active_users (view)"),
        "views must be marked with `(view)`:\n{frame}"
    );
}

#[test]
fn status_bar_reports_source_zoom_contents_selection_and_camera() {
    let status = status(&app());
    assert!(status.contains("demo schema (built in)"), "{status}");
    assert!(status.contains("zoom normal"), "{status}");
    assert!(status.contains("8 tables, 33 columns"), "{status}");
    assert!(status.contains("users"), "{status}");
    assert!(
        status.contains("camera 0,0"),
        "the camera starts at the diagram's top-left corner: {status}"
    );
    assert!(status.contains("? help · q quit"), "{status}");
}

#[test]
fn demo_frame_glyph_census_is_stable() {
    // Golden census of one 100x30 frame in the startup view (camera at the
    // diagram's top-left corner). Changing these numbers is a deliberate change
    // to the rendered diagram (or a broken toolchain — see the module comment).
    assert_eq!(
        census(&text_of(&app())),
        "─=322 │=93 ┌=12 ┐=12 └=8 ┘=11 ├=6 ┤=4 ┬=1 ┴=1"
    );
}

#[test]
fn demo_edges_are_drawn_and_the_primary_key_is_not_a_self_reference() {
    // Routed edges reach their tables: the frame carries connector corners and
    // junction glyphs, not only table borders.
    let frame = text_of(&app());
    let connectors: usize = ['┌', '┐', '└', '┘', '├', '┤', '┬', '┴']
        .iter()
        .map(|glyph| frame.matches(*glyph).count())
        .sum();
    assert!(
        connectors > 0,
        "no routed edge glyphs in the demo frame:\n{frame}"
    );

    // The categories self-reference must not turn its primary key into a key
    // that points at itself.
    let schema = demo_schema();
    let categories = schema.table("categories").expect("categories");
    assert_eq!(
        categories.column("id").expect("id").key,
        sqlv::model::KeyKind::Primary,
        "categories.id is marked as a foreign key onto itself"
    );
    assert_eq!(
        categories.column("parent_id").expect("parent_id").key,
        sqlv::model::KeyKind::Foreign
    );
}

/// A frame at a viewport large enough to hold the whole diagram, so these
/// assertions test the drawing rather than the scrolling.
fn zoomed_frame(keys: &str) -> (String, String) {
    let mut app = app_in(diagram_rect(200, 60));
    for code in keys.chars() {
        press(&mut app, KeyCode::Char(code));
    }
    (text_of_in(&app, 200, 60), status(&app))
}

#[test]
fn zoom_levels_keep_every_table_name_and_draw_their_junctions() {
    // At the farthest level the boxes are borderless and one row tall, so a
    // route ending *on* a box overwrites a character of the table name and the
    // table appears to vanish. The route must stop one cell short instead.
    for (keys, level) in [("", "normal"), ("-", "out1"), ("--", "out2")] {
        let (frame, status) = zoomed_frame(keys);
        assert!(status.contains(&format!("zoom {level}")), "{status}");
        for table in DEMO_TABLES {
            assert!(
                frame.contains(table),
                "`{table}` is missing at zoom {level}:\n{frame}"
            );
        }
        let junctions: usize = JUNCTIONS.iter().map(|g| frame.matches(*g).count()).sum();
        if level == "out2" {
            assert_eq!(
                junctions, 0,
                "a junction glyph landed on a borderless table at zoom {level}:\n{frame}"
            );
        } else {
            assert!(
                junctions > 0,
                "bordered boxes must still show junction glyphs at zoom {level}:\n{frame}"
            );
        }
    }
    // Golden census of the borderless name map: routed lines only, no junctions.
    assert_eq!(
        census(&zoomed_frame("--").0),
        "─=97 │=7 ┌=3 ┐=3 └=3 ┘=2 ├=0 ┤=0 ┬=0 ┴=0"
    );
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

#[test]
fn q_and_escape_ask_to_quit() {
    assert!(press(&mut app(), KeyCode::Char('q')), "`q` must quit");
    assert!(press(&mut app(), KeyCode::Esc), "`Esc` must quit");
    assert!(!press(&mut app(), KeyCode::Char('j')), "`j` must not quit");
}

#[test]
fn plus_and_minus_move_through_the_zoom_levels() {
    let mut app = app();
    assert!(status(&app).contains("zoom normal"), "{}", status(&app));

    press(&mut app, KeyCode::Char('+'));
    assert!(status(&app).contains("zoom in1"), "{}", status(&app));

    press(&mut app, KeyCode::Char('-'));
    press(&mut app, KeyCode::Char('-'));
    assert!(status(&app).contains("zoom out1"), "{}", status(&app));
}

#[test]
fn tab_and_shift_tab_walk_the_selection_and_move_the_highlight() {
    let mut app = app();
    assert!(status(&app).contains("users"), "{}", status(&app));
    let first = reversed_cells(&app);
    assert!(
        !first.is_empty(),
        "the selected table must be drawn in reverse video"
    );

    press(&mut app, KeyCode::Tab);
    assert!(status(&app).contains("categories"), "{}", status(&app));
    let second = reversed_cells(&app);
    assert_ne!(first, second, "the highlight must move with the selection");

    press_shift(&mut app, KeyCode::BackTab);
    assert!(status(&app).contains("users"), "{}", status(&app));
}

#[test]
fn help_overlay_lists_the_bindings_and_closes_on_any_other_key() {
    let mut app = app();
    let camera = status(&app);

    press(&mut app, KeyCode::Char('?'));
    let help = text_of(&app);
    for binding in [
        " keys ",
        "pan the diagram",
        "zoom in / out",
        "select the next / previous table",
        "search tables by name",
        "quit",
    ] {
        assert!(
            help.contains(binding),
            "help is missing `{binding}`:\n{help}"
        );
    }

    // Any other key closes the overlay and is not passed on to the diagram.
    press(&mut app, KeyCode::Char('j'));
    assert!(!contains(&app, " keys "), "help must close again");
    assert_eq!(
        status(&app),
        camera,
        "the closing key must not pan the view"
    );
    assert_eq!(
        reversed_cells(&app),
        reversed_cells(&app_in(diagram_rect(WIDTH, HEIGHT))),
        "the closing key must not change the selection highlight"
    );
}

#[test]
fn help_overlay_quits_on_q() {
    let mut app = app();
    press(&mut app, KeyCode::Char('?'));
    assert!(press(&mut app, KeyCode::Char('q')), "`q` in help must quit");
}

#[test]
fn search_prompt_swallows_letters_and_counts_matches() {
    let mut app = app();
    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "ord");

    let bar = status(&app);
    assert!(bar.contains("/ord"), "the query must be echoed: {bar}");
    assert!(
        bar.contains("2 match"),
        "`ord` matches orders and order_lines: {bar}"
    );
    assert!(
        bar.contains("Enter accept") && bar.contains("Esc cancel"),
        "{bar}"
    );

    // `q` is part of the query, not a quit.
    assert!(!press(&mut app, KeyCode::Char('q')), "`q` must be typed");
    assert!(status(&app).contains("/ordq"), "{}", status(&app));
    press(&mut app, KeyCode::Backspace);
    assert!(status(&app).contains("/ord "), "{}", status(&app));

    // Accepting the prompt goes back to the normal status bar.
    assert!(
        !press(&mut app, KeyCode::Enter),
        "Enter accepts, it does not quit"
    );
    assert!(status(&app).contains("zoom normal"), "{}", status(&app));
}

#[test]
fn search_prompt_can_be_cancelled_with_escape() {
    let mut app = app();
    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "ship");
    assert!(status(&app).contains("/ship"), "{}", status(&app));

    // Esc only drops the prompt: the search-as-you-type jump has already
    // scrolled to the match, and cancelling does not scroll back.
    assert!(
        !press(&mut app, KeyCode::Esc),
        "Esc cancels, it does not quit"
    );
    let bar = status(&app);
    assert!(!bar.contains("/ship"), "the prompt must be gone: {bar}");
    assert!(bar.contains("shipments"), "the match stays selected: {bar}");
}

#[test]
fn search_prompt_reports_no_matches_for_an_unknown_name() {
    let mut app = app();
    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "zzz");
    assert!(status(&app).contains("0 match"), "{}", status(&app));
}

// ---------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------

/// Every size either fills exactly or is clipped — never panics and never
/// bleeds into the next row.
#[test]
fn frames_fill_the_terminal_at_every_size() {
    let sizes = [
        (1u16, 1u16),
        (1, 40),
        (4, 3),
        (8, 4),
        (20, 3),
        (40, 10),
        (100, 30),
        (200, 3),
        (3, 200),
        (300, 60),
    ];
    for (width, height) in sizes {
        for label in ["diagram", "help", "search"] {
            let mut app = app_in(diagram_rect(width, height));
            match label {
                "help" => {
                    press(&mut app, KeyCode::Char('?'));
                }
                "search" => {
                    press(&mut app, KeyCode::Char('/'));
                    type_text(&mut app, "prod");
                }
                _ => {}
            }

            let buffer = buffer_of(&app, width, height);
            let area = buffer.area();
            assert_eq!(
                (area.width, area.height),
                (width, height),
                "backend size mismatch at {width}x{height} ({label})"
            );

            let frame = text(&buffer);
            let lines: Vec<&str> = frame.lines().collect();
            assert_eq!(
                lines.len(),
                height as usize,
                "wrong row count at {width}x{height} ({label}):\n{frame}"
            );
            for (row, line) in lines.iter().enumerate() {
                assert!(
                    line.chars().count() <= width as usize,
                    "row {row} is wider than {width} columns at {label}:\n{line:?}"
                );
            }
        }
    }
}

#[test]
fn resize_relayouts_and_keeps_the_frame_consistent() {
    let mut app = app();

    // Shrink far enough that the status bar's segments no longer fit whole.
    app.resize(diagram_rect(40, 10));
    let frame = text_of_in(&app, 40, 10);
    assert_eq!(frame.lines().count(), 10);
    assert!(
        frame.contains("demo schema (built in) · zoom normal"),
        "the status bar's leading segments must survive a narrow terminal:\n{frame}"
    );

    // Grow again and check the diagram is still there.
    app.resize(diagram_rect(120, 40));
    let frame = text_of_in(&app, 120, 40);
    assert!(
        frame.contains("order_lines"),
        "the diagram must still be there:\n{frame}"
    );
    // A resized app must draw exactly what a fresh app of the same size draws
    // (both are inspected through an equally sized backend).
    let fresh = text_of_in(&app_in(diagram_rect(120, 40)), 120, 40);
    assert_eq!(census(&frame), census(&fresh));
}

fn text_of_in(app: &App, width: u16, height: u16) -> String {
    text(&buffer_of(app, width, height))
}
