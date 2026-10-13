//! End-to-end SQLite introspection.
//!
//! The unit tests cover the pieces ([`normalize_url`], type normalisation,
//! identifier quoting); this test builds a real database on disk, reads it back
//! through the same entry points the binary uses ([`Target::resolve`] then
//! [`introspect`]) and checks the frozen model that the renderer consumes.

use sqlv::introspect::sqlite::{normalize_url, SqliteIntrospector};
use sqlv::introspect::{introspect, DriverKind, SchemaIntrospector, Target};
use sqlv::model::KeyKind;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

/// Every shape the viewer has to draw: a plain table, a composite primary key,
/// a composite/plain foreign key, a self reference and a view.
const DDL: &[&str] = &[
    "CREATE TABLE users (
         id INTEGER PRIMARY KEY,
         email TEXT NOT NULL,
         display_name TEXT
     )",
    "CREATE TABLE teams (
         id INTEGER PRIMARY KEY,
         name TEXT NOT NULL
     )",
    "CREATE TABLE memberships (
         user_id INTEGER NOT NULL REFERENCES users(id),
         team_id INTEGER NOT NULL REFERENCES teams(id),
         role TEXT NOT NULL,
         PRIMARY KEY (user_id, team_id)
     )",
    "CREATE TABLE orders (
         id INTEGER PRIMARY KEY,
         user_id INTEGER NOT NULL REFERENCES users(id),
         total_cents INTEGER NOT NULL DEFAULT 0,
         placed_at TEXT
     )",
    "CREATE TABLE order_lines (
         order_id INTEGER NOT NULL,
         line_no INTEGER NOT NULL,
         sku TEXT NOT NULL,
         qty INTEGER NOT NULL DEFAULT 1,
         PRIMARY KEY (order_id, line_no),
         FOREIGN KEY (order_id) REFERENCES orders(id)
     )",
    "CREATE TABLE categories (
         id INTEGER PRIMARY KEY,
         parent_id INTEGER REFERENCES categories(id),
         label TEXT NOT NULL
     )",
    "CREATE VIEW active_users AS SELECT id, email FROM users",
];

/// Write the fixture with a writable pool; the introspector itself only ever
/// opens databases read-only.
async fn write_fixture(path: &str) {
    let options = SqliteConnectOptions::from_str(&normalize_url(path))
        .expect("fixture url")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open fixture database");
    for statement in DDL {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{statement}\n{error}"));
    }
    pool.close().await;
}

#[tokio::test]
async fn a_sqlite_file_round_trips_through_the_introspector() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("fixture.db");
    let path = db.to_str().expect("utf-8 path");
    write_fixture(path).await;

    // The binary's path: infer the driver from the connection string, then hand
    // the resolved target to the shared dispatcher.
    let target = Target::resolve(path, None).expect("sqlite driver");
    assert_eq!(target.driver, DriverKind::Sqlite);
    let schema = introspect(&target).await.expect("introspection");

    // `sqlite_master` is read tables first (name order), then views.
    let names: Vec<&str> = schema
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "categories",
            "memberships",
            "order_lines",
            "orders",
            "teams",
            "users",
            "active_users",
        ]
    );

    let view = schema.table("active_users").expect("active_users");
    assert!(view.is_view);
    assert!(view.primary_key.is_empty());
    assert!(view.column("email").is_some());

    let users = schema.table("users").expect("users");
    assert!(!users.is_view);
    assert_eq!(users.primary_key, ["id"]);
    assert_eq!(users.column("id").expect("id").key, KeyKind::Primary);
    assert!(users.column("display_name").expect("display_name").nullable);
    assert!(!users.column("email").expect("email").nullable);

    let orders = schema.table("orders").expect("orders");
    assert_eq!(orders.primary_key, ["id"]);
    assert_eq!(orders.column("id").expect("id").key, KeyKind::Primary);
    assert_eq!(
        orders.column("user_id").expect("user_id").key,
        KeyKind::Foreign
    );
    assert_eq!(
        orders.column("total_cents").expect("total_cents").key,
        KeyKind::None
    );
    assert_eq!(
        orders
            .column("total_cents")
            .expect("total_cents")
            .default
            .as_deref(),
        Some("0")
    );
    assert!(orders.column("placed_at").expect("placed_at").nullable);
    assert!(!orders.column("user_id").expect("user_id").nullable);

    // A declared composite primary key survives in declaration order, and the
    // column that is also a foreign key reports both facts.
    let lines = schema.table("order_lines").expect("order_lines");
    assert_eq!(lines.primary_key, ["order_id", "line_no"]);
    assert_eq!(
        lines.column("order_id").expect("order_id").key,
        KeyKind::PrimaryForeign
    );
    assert_eq!(
        lines.column("line_no").expect("line_no").key,
        KeyKind::Primary
    );
    assert_eq!(lines.column("sku").expect("sku").key, KeyKind::None);

    // A join table without a declared primary key is inferred from its foreign
    // keys: the two leading columns become `PK,FK`, the payload column stays bare.
    let memberships = schema.table("memberships").expect("memberships");
    assert_eq!(memberships.primary_key, ["user_id", "team_id"]);
    assert_eq!(
        memberships.column("user_id").expect("user_id").key,
        KeyKind::PrimaryForeign
    );
    assert_eq!(
        memberships.column("team_id").expect("team_id").key,
        KeyKind::PrimaryForeign
    );
    assert_eq!(memberships.column("role").expect("role").key, KeyKind::None);

    // Foreign keys are kept, resolvable and complete.
    let orders_fk = schema
        .foreign_keys
        .iter()
        .find(|fk| fk.from_table == "orders")
        .expect("orders foreign key");
    assert_eq!(orders_fk.from_columns, ["user_id"]);
    assert_eq!(orders_fk.to_table, "users");
    assert_eq!(orders_fk.to_columns, ["id"]);
    assert!(!orders_fk.is_composite());

    let lines_fk = schema
        .foreign_keys
        .iter()
        .find(|fk| fk.from_table == "order_lines")
        .expect("order_lines foreign key");
    assert_eq!(lines_fk.from_columns, ["order_id"]);
    assert_eq!(lines_fk.to_table, "orders");
    assert_eq!(lines_fk.to_columns, ["id"]);

    let self_fk = schema
        .foreign_keys
        .iter()
        .find(|fk| fk.from_table == "categories")
        .expect("categories self reference");
    assert_eq!(self_fk.from_columns, ["parent_id"]);
    assert_eq!(self_fk.to_table, "categories");
    assert_eq!(self_fk.to_columns, ["id"]);

    // Every constraint declared above is present, and none points at a table
    // that does not exist (retain_resolvable_foreign_keys ran): memberships
    // contributes two, orders one, order_lines one and categories one.
    assert_eq!(schema.foreign_keys.len(), 5);
    for fk in &schema.foreign_keys {
        assert!(schema.table(&fk.from_table).is_some(), "{fk:?}");
        assert!(schema.table(&fk.to_table).is_some(), "{fk:?}");
        assert_eq!(fk.from_columns.len(), fk.to_columns.len(), "{fk:?}");
    }
}

#[tokio::test]
async fn the_introspector_describes_itself_without_touching_the_file() {
    let introspector = SqliteIntrospector::new("/tmp/never-opened.db");
    assert_eq!(introspector.describe(), "sqlite · /tmp/never-opened.db");
}

#[tokio::test]
async fn an_explicit_driver_override_wins_over_the_path() {
    let target = Target::resolve("/tmp/not-a-url.db", Some("sqlite")).expect("override");
    assert_eq!(target.driver, DriverKind::Sqlite);
    assert_eq!(target.url, "/tmp/not-a-url.db");
}
