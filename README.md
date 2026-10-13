# sqlv

A keyboard-driven terminal ER diagram viewer. It connects to a live SQL database,
introspects its tables, columns, primary keys and foreign keys, and draws the
result as a pannable, zoomable diagram of boxes and connector lines — no browser,
no export step.

```
┌ users ───────────────────────────────┐
│ id            integer   PK           │
│ email         text      NOT NULL     │
│ display_name  text                   │
└──────────────────────────────────────┘
              │ fk_orders_user_id
              ▼
┌ orders ──────────────────────────────┐
│ id            integer   PK           │
│ user_id       integer   FK NOT NULL  │
└──────────────────────────────────────┘
```

Status bar: `postgresql · app.db · zoom normal · 8 tables, 33 columns · orders · camera 0,0`

## Quick start

```console
$ sqlv --demo
```

`--demo` draws a built-in fixture schema (tables, views, composite keys, a
self-referencing foreign key) so you can try the viewer without a database. The
demo path is a permanent feature, not scaffolding.

## Build

The project is a Crane/Rust flake.

```console
$ nix develop          # dev shell: rustc 1.99.0, cargo, clippy, rustfmt, sqlite3, psql, mariadb
$ cargo build          # or: cargo run -- --demo
$ cargo test           # 104 unit tests + 3 SQLite integration + 13 UI tests
$ cargo clippy --all-targets
```

`nix build` and `nix run` work out of the box: Crane (`craneLib.buildPackage`)
vendors the dependencies straight from the committed `Cargo.lock`, so no
`cargoHash` is needed. A release build (`cargo build --release`) uses thin LTO
and `opt-level = 1`; the latter works around a rustc 1.99.0 codegen miscompile
at `opt-level >= 2` (see the comment in `Cargo.toml`).

## Install

The installed command is `sqlv`.

**Nix flake (primary):**

```console
$ nix profile install .        # installs ~/.nix-profile/bin/sqlv
$ nix build                    # build only: ./result/bin/sqlv
$ nix run . -- --demo          # run without installing
```

**Portable (any platform with a Rust toolchain):**

```console
$ cargo install --path .       # installs ~/.cargo/bin/sqlv
```

**NixOS / home-manager:** add this repository as a flake input and put the
package in `environment.systemPackages` (or `home.packages`):

```nix
environment.systemPackages = [ inputs.sqlv.packages.${pkgs.system}.default ];
```

## Usage

```
sqlv [OPTIONS] [URL]

OPTIONS:
    --demo             Draw the built-in fixture schema; no database needed
    --url <URL>        Connection string (positional URLs are accepted too)
    --db <DRIVER>      Force the driver when the URL scheme is ambiguous:
                       sqlite, postgres, mysql, mariadb, mssql
    -h, --help         Print this help
    -V, --version      Print the version
```

The driver is inferred from the URL scheme:

| scheme | driver | example |
| --- | --- | --- |
| `sqlite:`, `sqlite://`, `file:`, bare path | SQLite | `sqlv app.db` |
| `postgres://`, `postgresql://`, `pg://` | PostgreSQL | `sqlv postgres://user:secret@localhost/app` |
| `mysql://` | MySQL | `sqlv mysql://user:secret@localhost/app` |
| `mariadb://` | MariaDB | `sqlv mariadb://user:secret@localhost/app` |
| `mssql://`, `sqlserver://`, `tds://` | SQL Server | `sqlv 'mssql://sa:secret@localhost:1433/app'` |

Use `--db` when the string is ambiguous, for example a bare path that is not a
SQLite file or a SQL Server ADO connection string (`--db mssql` also accepts a
`Server=…;Database=…` string).

### Keys

| key | action |
| --- | --- |
| `h` `j` `k` `l` / arrows | pan the diagram |
| `+` / `-` | zoom in / out |
| `g` / `G` | jump to the top / bottom |
| `Ctrl+d` / `Ctrl+u` | page down / up |
| `Tab` / `Shift+Tab` | select the next / previous table |
| `/` | search tables by name |
| `?` | toggle the key help overlay |
| `q` / `Esc` | quit |

The viewer needs an interactive terminal: when stdout is not a TTY it prints
`standard output is not a terminal` and exits instead of corrupting the screen
(`--help` and `--version` work anywhere).

## How it works

- **One model, one interface.** Every backend returns the same frozen model —
  `Schema { tables, foreign_keys }` with `Table`, `Column`, `KeyKind` and
  `ForeignKey`. The renderer knows nothing about SQL. Composite primary and
  foreign keys are modelled from day one.
- **Runtime SQL, not macros.** Introspection uses the sqlx runtime API only, so
  there is no compile-time schema checking and no `.sqlx/` cache or
  `DATABASE_URL` at build time. SQLite and PostgreSQL/MySQL/MariaDB go through
  sqlx; SQL Server goes through tiberius (TDS).
- **Strict world/screen split.** Coordinates handed to the painter are world
  coordinates; `Camera` is the only thing that converts them to screen cells, and
  `Canvas::put` is the only place that clips. Panning and centring are therefore
  two lines of arithmetic, and nothing else can accidentally depend on the
  viewport.
- **Zoom is level-of-detail, not character scaling.** Four levels
  (`out2`, `out1`, `normal`, `in1`) are shown in the status bar. Zooming
  re-measures every table box with different padding, gaps and detail rows, then
  re-centres the viewport on the same world point, so text stays legible.
- **Deterministic layout.** Tables are packed into a reproducible grid and
  foreign keys are routed as orthogonal, A*-searched paths that avoid the boxes,
  with a deterministic detour when no path exists. No physics, no randomness.

Each backend reads catalog metadata only: SQLite via `PRAGMA table_info` /
`foreign_key_list` / `sqlite_master`, PostgreSQL via `pg_catalog` and
`information_schema`, MySQL/MariaDB and SQL Server via their `information_schema`
/ `sys.*` equivalents. The SQLite driver additionally opens the file read-only.

## Tests

```console
$ nix develop --command cargo test
```

- `src/**` unit tests (104) cover the model, canvas/camera clipping, zoom
  levels, table measurement and drawing, grid layout, orthogonal routing and CLI
  parsing — all without a database.
- `tests/sqlite_introspection.rs` (3) builds a real SQLite fixture (six tables,
  a view, composite keys, a self-referencing FK) in a temporary directory and
  checks the full introspect → model round trip, plus driver resolution and
  `--db` override.
- `tests/ui_render.rs` (13) drives the real `App` through a ratatui
  `TestBackend`: it presses keys, renders frames and asserts on the text and the
  styles a user would see — status bar, selection highlight, zoom levels, help
  overlay, search prompt, narrow-terminal behaviour, and a golden census of the
  box-drawing glyphs in the demo diagram's startup frame. No TTY, no timing and
  no database, so it runs anywhere `cargo test` runs.

To look at a frame yourself, or to diff two builds, the same rendering path is
exposed as a text dumper:

```console
$ cargo run --example dump_frame -- --size 100x30 --keys "/ord\r"
$ diff <(cargo run -q --example dump_frame) \
       <(cargo run -q --release --example dump_frame)
```

The glyph census is also the cross-profile canary for the rustc 1.99.0 codegen
miscompile described in `Cargo.toml`: if it fails in one profile only, suspect
the toolchain before suspecting the drawing code.

## Not included

The viewer is read-only and single-schema: it never writes to the database and
does not diff two schemas. Highlighting a foreign key when its table is selected,
diagram export (PNG/SVG) and schema diffing are deliberately parked.

## License

MIT.
