//! Print one rendered frame of the viewer as plain text — no terminal needed.
//!
//! This is the bridge between the interactive TUI and an ordinary test or diff.
//! It drives the same [`App`] the real viewer uses (same layout, same canvas,
//! same status bar, same key handling) but captures the frame into an
//! in-memory [`TestBackend`] buffer instead of drawing to a TTY, then prints
//! that buffer to stdout.
//!
//! ```console
//! $ cargo run --example dump_frame -- --size 100x30
//! $ cargo run --example dump_frame -- --size 100x30 --keys "jj+?" > frame.txt
//! $ cargo run --example dump_frame -- --size 24x80 --keys "/ord\r"
//! ```
//!
//! `--size` defaults to `100x30` and `--keys` to no input at all, so the same
//! invocation always prints exactly the same text — the demo schema has no
//! randomness, no clock and no database. That makes it usable as a golden-file
//! or diff tool, including across build profiles:
//!
//! ```console
//! $ diff <(cargo run -q --example dump_frame) \
//!        <(cargo run -q --release --example dump_frame)
//! ```
//!
//! Key notation: printable characters stand for themselves, `\t` is Tab,
//! `\r` is Enter and `\e` is Esc. Anything else after a backslash is an error,
//! so a typo cannot silently replay the wrong input.

use std::process::ExitCode;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::Terminal;

use sqlv::app::App;
use sqlv::introspect::demo::demo_schema;

const USAGE: &str = "\
usage: dump_frame [--size WxH] [--keys KEYS]

  --size WxH   terminal size to render, default 100x30
  --keys KEYS  keys to replay before drawing, default none
               (\\t Tab, \\r Enter, \\e Esc, \\\\ backslash)
";

const DEFAULT_SIZE: (u16, u16) = (100, 30);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dump_frame: {message}");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (size, keys) = parse(args)?;
    let viewport = Rect::new(0, 0, size.0, size.1);
    let mut app = App::new(
        &demo_schema(),
        "demo schema (built in)".to_string(),
        viewport,
    );

    let mut quit_after = None;
    for (index, key) in keys.iter().enumerate() {
        if app.handle_key(*key) {
            quit_after = Some((index, *key));
            break;
        }
    }

    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1))
        .map_err(|error| format!("could not create the in-memory terminal: {error}"))?;
    terminal
        .draw(|frame| app.draw(frame))
        .map_err(|error| format!("could not draw a frame: {error}"))?;

    println!("{}", text(terminal.backend().buffer()));

    if let Some((index, key)) = quit_after {
        eprintln!("dump_frame: key {index} ({key:?}) asked to quit; frame printed anyway");
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<((u16, u16), Vec<KeyEvent>), String> {
    let mut size = DEFAULT_SIZE;
    let mut keys = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1);
        match args[index].as_str() {
            "--size" => {
                size = parse_size(value.ok_or("--size needs a value like 100x30")?)?;
                index += 2;
            }
            "--keys" => {
                keys = parse_keys(value.ok_or("--keys needs a value")?)?;
                index += 2;
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok((size, keys))
}

fn parse_size(value: &str) -> Result<(u16, u16), String> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("`{value}` is not a size like 100x30"))?;
    let width: u16 = width
        .parse()
        .map_err(|_| format!("`{width}` is not a width"))?;
    let height: u16 = height
        .parse()
        .map_err(|_| format!("`{height}` is not a height"))?;
    if width == 0 || height == 0 {
        return Err("a size of zero has no frame to draw".to_string());
    }
    Ok((width, height))
}

fn parse_keys(value: &str) -> Result<Vec<KeyEvent>, String> {
    let mut keys = Vec::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        let code = match character {
            '\\' => match characters.next() {
                Some('t') => KeyCode::Tab,
                Some('r') => KeyCode::Enter,
                Some('e') => KeyCode::Esc,
                Some('\\') => KeyCode::Char('\\'),
                Some(other) => return Err(format!("unknown escape `\\{other}`")),
                None => return Err("a trailing backslash escapes nothing".to_string()),
            },
            character => KeyCode::Char(character),
        };
        keys.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    Ok(keys)
}

/// The frame as text: one line per terminal row, trailing blanks trimmed.
fn text(buffer: &Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            let line: String = (0..area.width)
                .map(|x| {
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
