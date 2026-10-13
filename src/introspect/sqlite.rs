//! SQLite introspection.
//!
//! SQLite has no information schema, so we read `sqlite_master` for the object
//! list and then run the pragmas each object carries: `table_info` for columns and
//! `foreign_key_list` for relationships. The connection is opened read-only, so a
//! mistyped path reports an error instead of silently creating a new database.

use std::collections::BTreeMap;
use std::str::FromStr;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;

use super::{IntrospectError, SchemaIntrospector};
use crate::model::{Column, ForeignKey, Schema, Table};

/// Reads the schema of a SQLite database file.
#[derive(Debug, Clone)]
pub struct SqliteIntrospector {
    url: String,
}

impl SqliteIntrospector {
    /// Point at a SQLite file. The connection is opened lazily by [`load`].
    ///
    /// [`load`]: SchemaIntrospector::load
    pub fn new(url: impl Into<String>) -> Self {
        SqliteIntrospector { url: url.into() }
    }

    /// The connection string as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Open the database read-only, without creating it when it is missing.
    pub async fn connect(&self) -> Result<SqlitePool, IntrospectError> {
        let normalized = normalize_url(&self.url);
        let options = SqliteConnectOptions::from_str(&normalized)
            .map_err(|error| IntrospectError::BadUrl {
                url: super::redact(&self.url),
                reason: error.to_string(),
            })?
            .read_only(true)
            .create_if_missing(false);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|error| IntrospectError::connect(&self.url, explain(&error)))
    }
}

#[async_trait]
impl SchemaIntrospector for SqliteIntrospector {
    async fn load(&self) -> Result<Schema, IntrospectError> {
        load_schema(&self.connect().await?).await
    }

    fn describe(&self) -> String {
        let shown = self
            .url
            .trim()
            .trim_start_matches("sqlite://")
            .trim_start_matches("sqlite:")
            .trim_start_matches("file:");
        format!("sqlite · {shown}")
    }
}

/// Turn a path or connection string into something sqlx accepts.
///
/// `app.db`, `/tmp/app.db`, `sqlite://app.db`, `sqlite:///tmp/app.db` and
/// `sqlite::memory:` pass through unchanged or gain a `sqlite://` prefix when they
/// are a bare path. A `file:` URI keeps its query parameters but takes the `sqlite:`
/// scheme sqlx expects.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("file:") {
        return format!("sqlite:{rest}");
    }
    if url.starts_with("sqlite:") {
        return url.to_string();
    }
    format!("sqlite://{url}")
}

/// The declared type to show in the box.
///
/// SQLite stores whatever the `CREATE TABLE` said, so there is no vocabulary to
/// normalize against; we only take care of the untyped case.
pub fn normalize_type(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        None | Some("") => "any".to_string(),
        Some(text) => text.to_string(),
    }
}

/// A connection failure usually means "no such file" for SQLite; say so plainly.
fn explain(error: &sqlx::Error) -> String {
    let text = error.to_string();
    if text.contains("unable to open database file") {
        format!("{text} (does the file exist, and is it readable?)")
    } else if text.contains("not a database") {
        format!("{text} (is this really a SQLite database?)")
    } else {
        text
    }
}

/// Quote an identifier for a pragma, which cannot take a bound parameter.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// One column of one `foreign_key_list` constraint.
#[derive(Debug, Clone)]
struct RawForeignKey {
    id: i64,
    seq: i64,
    to_table: String,
    from_column: String,
    to_column: Option<String>,
}

/// Read the whole schema of the database behind `pool`.
pub async fn load_schema(pool: &SqlitePool) -> Result<Schema, IntrospectError> {
    let objects = list_objects(pool).await?;
    let mut schema = Schema::new();

    for (name, is_view) in &objects {
        let (columns, primary_key) = read_columns(pool, name).await?;
        let mut table = Table::new(name)
            .with_columns(columns)
            .with_primary_key(primary_key);
        if *is_view {
            table = table.as_view();
        }
        schema.tables.push(table);
    }

    // Foreign keys are read once every table exists, so a constraint that omits
    // its target columns can be completed from the target's primary key.
    for (name, is_view) in &objects {
        if *is_view {
            continue;
        }
        for raw in read_foreign_keys(pool, name).await? {
            let to_columns = match &raw.to_column {
                Some(column) => vec![column.clone()],
                None => implicit_target_columns(&schema, &raw.to_table),
            };
            if to_columns.is_empty() {
                continue;
            }
            schema.foreign_keys.push(
                ForeignKey::composite(
                    name.as_str(),
                    vec![raw.from_column],
                    raw.to_table.as_str(),
                    to_columns,
                )
                .named(format!("fk_{}_{}", name, raw.id)),
            );
        }
    }

    schema.retain_resolvable_foreign_keys();
    schema.finalize_keys();
    Ok(schema)
}

/// Tables and views in `main`, tables first, each group in name order.
async fn list_objects(pool: &SqlitePool) -> Result<Vec<(String, bool)>, IntrospectError> {
    let rows = sqlx::query(
        "SELECT name, type FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
         ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| {
        IntrospectError::query("read sqlite_master (is this a SQLite file?)", error)
    })?;

    let mut objects = Vec::with_capacity(rows.len());
    for row in rows {
        let name = text(&row, "name")?.unwrap_or_default();
        let kind = text(&row, "type")?.unwrap_or_default();
        objects.push((name, kind.eq_ignore_ascii_case("view")));
    }
    Ok(objects)
}

/// Columns of one table or view, plus its primary key in declaration order.
async fn read_columns(
    pool: &SqlitePool,
    table: &str,
) -> Result<(Vec<Column>, Vec<String>), IntrospectError> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|error| IntrospectError::query(format!("PRAGMA table_info({table})"), error))?;

    let mut columns = Vec::with_capacity(rows.len());
    let mut primary_key: Vec<(i64, String)> = Vec::new();

    for row in rows {
        let name = text(&row, "name")?.unwrap_or_default();
        let data_type = normalize_type(text(&row, "type")?.as_deref());
        let not_null = integer(&row, "notnull")? != 0;
        let default = text(&row, "dflt_value")?.filter(|value| !value.is_empty());
        let pk_position = integer(&row, "pk")?;
        if pk_position > 0 {
            primary_key.push((pk_position, name.clone()));
        }
        let mut column = Column::new(name, data_type).nullable(!not_null);
        if let Some(default) = default {
            column = column.with_default(default);
        }
        columns.push(column);
    }

    primary_key.sort_by_key(|(position, _)| *position);
    Ok((
        columns,
        primary_key.into_iter().map(|(_, name)| name).collect(),
    ))
}

/// The columns of every foreign key declared on one table, in declaration order.
async fn read_foreign_keys(
    pool: &SqlitePool,
    table: &str,
) -> Result<Vec<RawForeignKey>, IntrospectError> {
    let sql = format!("PRAGMA foreign_key_list({})", quote_ident(table));
    let rows = sqlx::query(&sql).fetch_all(pool).await.map_err(|error| {
        IntrospectError::query(format!("PRAGMA foreign_key_list({table})"), error)
    })?;

    let mut grouped: BTreeMap<i64, Vec<RawForeignKey>> = BTreeMap::new();
    for row in rows {
        let id = integer(&row, "id")?;
        grouped.entry(id).or_default().push(RawForeignKey {
            id,
            seq: integer(&row, "seq")?,
            to_table: text(&row, "table")?.unwrap_or_default(),
            from_column: text(&row, "from")?.unwrap_or_default(),
            to_column: text(&row, "to")?.filter(|value| !value.is_empty()),
        });
    }

    // One constraint per `id`; its rows are the columns of a composite key.
    let mut foreign_keys = Vec::new();
    for (_, mut group) in grouped {
        group.sort_by_key(|raw| raw.seq);
        foreign_keys.extend(group);
    }
    Ok(foreign_keys)
}

/// The columns a constraint targets when SQLite does not name them: the primary key.
fn implicit_target_columns(schema: &Schema, table: &str) -> Vec<String> {
    match schema.table_loose(table) {
        Some(target) if !target.primary_key.is_empty() => target.primary_key.clone(),
        Some(target) if target.columns.len() == 1 => vec![target.columns[0].name.clone()],
        _ => Vec::new(),
    }
}

fn text(row: &SqliteRow, column: &str) -> Result<Option<String>, IntrospectError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|error| IntrospectError::query(format!("sqlite column {column}"), error))
}

fn integer(row: &SqliteRow, column: &str) -> Result<i64, IntrospectError> {
    row.try_get::<i64, _>(column)
        .map_err(|error| IntrospectError::query(format!("sqlite column {column}"), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_paths_become_sqlite_urls() {
        assert_eq!(normalize_url("app.db"), "sqlite://app.db");
        assert_eq!(normalize_url("  app.db  "), "sqlite://app.db");
        assert_eq!(normalize_url("/tmp/app.db"), "sqlite:///tmp/app.db");
        assert_eq!(normalize_url("./app.db"), "sqlite://./app.db");
        assert_eq!(normalize_url("sqlite://app.db"), "sqlite://app.db");
        assert_eq!(
            normalize_url("sqlite:///tmp/app.db"),
            "sqlite:///tmp/app.db"
        );
        assert_eq!(normalize_url("sqlite::memory:"), "sqlite::memory:");
    }

    #[test]
    fn file_uris_take_the_sqlite_scheme_but_keep_their_parameters() {
        assert_eq!(normalize_url("file:/tmp/app.db"), "sqlite:/tmp/app.db");
        assert_eq!(
            normalize_url("file:app.db?mode=ro"),
            "sqlite:app.db?mode=ro"
        );
    }

    #[test]
    fn untyped_columns_are_labelled_any() {
        assert_eq!(normalize_type(None), "any");
        assert_eq!(normalize_type(Some("")), "any");
        assert_eq!(normalize_type(Some("   ")), "any");
    }

    #[test]
    fn declared_types_are_shown_verbatim_apart_from_whitespace() {
        assert_eq!(normalize_type(Some("VARCHAR(255)")), "VARCHAR(255)");
        assert_eq!(normalize_type(Some("  INTEGER ")), "INTEGER");
        assert_eq!(normalize_type(Some("unsigned big int")), "unsigned big int");
    }

    #[test]
    fn identifiers_are_quoted_and_escaped() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn a_missing_file_is_reported_as_a_connection_problem() {
        let message = explain(&sqlx::Error::Protocol(
            "unable to open database file".into(),
        ));
        assert!(message.contains("does the file exist"), "{message}");
    }

    #[test]
    fn implicit_target_columns_fall_back_to_the_primary_key() {
        let mut schema = Schema::new();
        schema.tables.push(
            Table::new("orders")
                .with_primary_key(["id"])
                .with_columns(vec![Column::new("id", "integer")]),
        );
        assert_eq!(
            implicit_target_columns(&schema, "orders"),
            vec!["id".to_string()]
        );
        assert!(implicit_target_columns(&schema, "missing").is_empty());
    }
}
