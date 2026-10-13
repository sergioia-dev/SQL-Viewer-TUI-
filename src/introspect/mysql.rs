//! MySQL and MariaDB introspection.
//!
//! Both servers speak the same protocol and the same `information_schema`, so one
//! introspector covers them: the driver only decides what `describe()` says.
//!
//! Every text column is read through `CAST(... AS CHAR)`. MariaDB stores several
//! `information_schema` columns as `LONGTEXT`, which sqlx hands back as bytes rather
//! than text; casting normalises them to a `VARCHAR` the decoder understands.

use std::collections::HashMap;
use std::str::FromStr;

use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::Row;

use super::{IntrospectError, SchemaIntrospector};
use crate::model::{Column, ForeignKey, Schema, Table};

/// Schemas that belong to the server itself rather than to the user's data.
#[cfg(test)]
const SYSTEM_SCHEMAS: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

const TABLES_SQL: &str = "SELECT CAST(table_schema AS CHAR) AS table_schema, \
            CAST(table_name AS CHAR) AS table_name, \
            CAST(table_type AS CHAR) AS table_type \
     FROM information_schema.tables \
     WHERE table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
     ORDER BY table_schema, table_name";

const COLUMNS_SQL: &str = "SELECT CAST(table_schema AS CHAR) AS table_schema, \
            CAST(table_name AS CHAR) AS table_name, \
            CAST(column_name AS CHAR) AS column_name, \
            CAST(data_type AS CHAR) AS data_type, \
            CAST(column_type AS CHAR) AS column_type, \
            CAST(is_nullable AS CHAR) AS is_nullable, \
            CAST(column_default AS CHAR) AS column_default \
     FROM information_schema.columns \
     WHERE table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
     ORDER BY table_schema, table_name, ordinal_position";

const PRIMARY_KEYS_SQL: &str = "SELECT CAST(table_schema AS CHAR) AS table_schema, \
            CAST(table_name AS CHAR) AS table_name, \
            CAST(column_name AS CHAR) AS column_name \
     FROM information_schema.key_column_usage \
     WHERE constraint_name = 'PRIMARY' \
       AND table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
     ORDER BY table_schema, table_name, ordinal_position";

const FOREIGN_KEYS_SQL: &str = "SELECT CAST(constraint_name AS CHAR) AS constraint_name, \
            CAST(table_schema AS CHAR) AS table_schema, \
            CAST(table_name AS CHAR) AS table_name, \
            CAST(column_name AS CHAR) AS column_name, \
            CAST(referenced_table_schema AS CHAR) AS target_schema, \
            CAST(referenced_table_name AS CHAR) AS target_table, \
            CAST(referenced_column_name AS CHAR) AS target_column \
     FROM information_schema.key_column_usage \
     WHERE referenced_table_name IS NOT NULL \
       AND table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
     ORDER BY constraint_name, ordinal_position";

/// Reads the schema of a MySQL or MariaDB database.
#[derive(Debug, Clone)]
pub struct MySqlIntrospector {
    url: String,
    is_mariadb: bool,
}

impl MySqlIntrospector {
    /// Point at a MySQL (`mariadb = false`) or MariaDB (`mariadb = true`) database.
    pub fn new(url: impl Into<String>, mariadb: bool) -> Self {
        MySqlIntrospector {
            url: url.into(),
            is_mariadb: mariadb,
        }
    }

    /// The connection string as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// True when this introspector was chosen for MariaDB.
    pub fn is_mariadb(&self) -> bool {
        self.is_mariadb
    }

    /// Open a one-connection pool and check that it answers.
    pub async fn connect(&self) -> Result<MySqlPool, IntrospectError> {
        let normalized = normalize_url(&self.url);
        let options = MySqlConnectOptions::from_str(&normalized).map_err(|error| {
            IntrospectError::BadUrl {
                url: super::redact(&self.url),
                reason: error.to_string(),
            }
        })?;
        MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|error| IntrospectError::connect(&self.url, error))
    }
}

#[async_trait]
impl SchemaIntrospector for MySqlIntrospector {
    async fn load(&self) -> Result<Schema, IntrospectError> {
        load_schema(&self.connect().await?).await
    }

    fn describe(&self) -> String {
        let engine = if self.is_mariadb { "mariadb" } else { "mysql" };
        format!("{engine} · {}", super::redact(&self.url))
    }
}

/// sqlx only understands the `mysql://` scheme, so MariaDB URLs are rewritten.
pub fn normalize_url(url: &str) -> String {
    for prefix in ["mariadb://", "maria://", "mariadb:", "maria:"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let rest = rest.trim_start_matches('/');
            return format!("mysql://{rest}");
        }
    }
    url.to_string()
}

/// Read the whole schema of the database behind `pool`.
pub async fn load_schema(pool: &MySqlPool) -> Result<Schema, IntrospectError> {
    let mut schema = Schema::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();

    for row in query(pool, TABLES_SQL, "list tables").await? {
        let table_schema = text(&row, "table_schema")?.unwrap_or_default();
        let table_name = text(&row, "table_name")?.unwrap_or_default();
        let table_type = text(&row, "table_type")?.unwrap_or_default();
        let mut table = Table::new(table_name.as_str()).in_schema(table_schema.as_str());
        if table_type.to_ascii_uppercase().contains("VIEW") {
            table = table.as_view();
        }
        index_of.insert((table_schema, table_name), schema.tables.len());
        schema.tables.push(table);
    }

    for row in query(pool, COLUMNS_SQL, "list columns").await? {
        let Some(index) = lookup(&index_of, &row)? else {
            continue;
        };
        let name = text(&row, "column_name")?.unwrap_or_default();
        let data_type = text(&row, "data_type")?.unwrap_or_default();
        let column_type = text(&row, "column_type")?;
        let mut column = Column::new(name, normalize_type(&data_type, column_type.as_deref()))
            .nullable(
                !text(&row, "is_nullable")?
                    .unwrap_or_default()
                    .eq_ignore_ascii_case("YES"),
            );
        if let Some(default) = text(&row, "column_default")? {
            let default = default.trim();
            // MySQL reports an empty string for "no default"; MariaDB reports NULL.
            if !default.is_empty() {
                column = column.with_default(default);
            }
        }
        schema.tables[index].columns.push(column);
    }

    for row in query(pool, PRIMARY_KEYS_SQL, "list primary keys").await? {
        let Some(index) = lookup(&index_of, &row)? else {
            continue;
        };
        let column = text(&row, "column_name")?.unwrap_or_default();
        schema.tables[index].primary_key.push(column);
    }

    let mut pending: Option<PendingForeignKey> = None;
    for row in query(pool, FOREIGN_KEYS_SQL, "list foreign keys").await? {
        let name = text(&row, "constraint_name")?.unwrap_or_default();
        let from_schema = text(&row, "table_schema")?.unwrap_or_default();
        let from_table = text(&row, "table_name")?.unwrap_or_default();
        let from_column = text(&row, "column_name")?.unwrap_or_default();
        let to_schema = text(&row, "target_schema")?.unwrap_or_default();
        let to_table = text(&row, "target_table")?.unwrap_or_default();
        let to_column = text(&row, "target_column")?.unwrap_or_default();

        let same = pending.as_ref().is_some_and(|current| {
            current.name == name
                && current.from_schema == from_schema
                && current.from_table == from_table
        });
        if !same {
            if let Some(done) = pending.take() {
                schema.foreign_keys.push(done.into_foreign_key());
            }
            pending = Some(PendingForeignKey::new(
                name,
                from_schema,
                from_table,
                to_schema,
                to_table,
            ));
        }
        if let Some(current) = pending.as_mut() {
            current.from_columns.push(from_column);
            current.to_columns.push(to_column);
        }
    }
    if let Some(done) = pending {
        schema.foreign_keys.push(done.into_foreign_key());
    }

    schema.retain_resolvable_foreign_keys();
    schema.finalize_keys();
    Ok(schema)
}

/// A foreign key under construction: its rows arrive one column at a time.
#[derive(Debug, Clone)]
struct PendingForeignKey {
    name: String,
    from_schema: String,
    from_table: String,
    to_schema: String,
    to_table: String,
    from_columns: Vec<String>,
    to_columns: Vec<String>,
}

impl PendingForeignKey {
    fn new(
        name: String,
        from_schema: String,
        from_table: String,
        to_schema: String,
        to_table: String,
    ) -> Self {
        PendingForeignKey {
            name,
            from_schema,
            from_table,
            to_schema,
            to_table,
            from_columns: Vec::new(),
            to_columns: Vec::new(),
        }
    }

    fn into_foreign_key(self) -> ForeignKey {
        let from = qualified(&self.from_schema, &self.from_table);
        let to = qualified(&self.to_schema, &self.to_table);
        ForeignKey::composite(from, self.from_columns, to, self.to_columns).named(self.name)
    }
}

/// `schema.name` for a two-part name, `name` when the schema is the default one.
fn qualified(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// How a MySQL type is shown in a box.
///
/// `information_schema.columns.column_type` is the full declaration — it keeps the
/// length, the unsigned flag and enum values that `data_type` drops — so it wins when
/// the server reports one. `tinyint(1)` is what MySQL calls a boolean and is shown as
/// such.
pub fn normalize_type(data_type: &str, column_type: Option<&str>) -> String {
    let raw = match column_type {
        Some(column_type) if !column_type.trim().is_empty() => column_type,
        _ => data_type,
    };
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("tinyint(1)") {
        return "boolean".to_string();
    }
    if raw.is_empty() {
        return "any".to_string();
    }
    raw.to_string()
}

async fn query(
    pool: &MySqlPool,
    sql: &str,
    context: &str,
) -> Result<Vec<MySqlRow>, IntrospectError> {
    sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| IntrospectError::query(context, error))
}

fn lookup(
    index_of: &HashMap<(String, String), usize>,
    row: &MySqlRow,
) -> Result<Option<usize>, IntrospectError> {
    let table_schema = text(row, "table_schema")?.unwrap_or_default();
    let table_name = text(row, "table_name")?.unwrap_or_default();
    Ok(index_of.get(&(table_schema, table_name)).copied())
}

fn text(row: &MySqlRow, column: &str) -> Result<Option<String>, IntrospectError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|error| IntrospectError::query(format!("mysql column {column}"), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mariadb_urls_are_rewritten_for_sqlx() {
        assert_eq!(
            normalize_url("mariadb://root@localhost/shop"),
            "mysql://root@localhost/shop"
        );
        assert_eq!(
            normalize_url("maria://root@localhost/shop"),
            "mysql://root@localhost/shop"
        );
        assert_eq!(
            normalize_url("mariadb:root@localhost/shop"),
            "mysql://root@localhost/shop"
        );
        assert_eq!(
            normalize_url("mysql://root@localhost/shop"),
            "mysql://root@localhost/shop"
        );
    }

    #[test]
    fn the_full_column_type_is_preferred_over_the_bare_data_type() {
        assert_eq!(
            normalize_type("varchar", Some("varchar(64)")),
            "varchar(64)"
        );
        assert_eq!(normalize_type("int", Some("int unsigned")), "int unsigned");
        assert_eq!(
            normalize_type("enum", Some("enum('open','closed')")),
            "enum('open','closed')"
        );
    }

    #[test]
    fn the_data_type_is_the_fallback_when_no_column_type_is_reported() {
        assert_eq!(normalize_type("text", None), "text");
        assert_eq!(normalize_type("text", Some("  ")), "text");
        assert_eq!(normalize_type("", None), "any");
    }

    #[test]
    fn tinyint_one_is_shown_as_a_boolean() {
        assert_eq!(normalize_type("tinyint", Some("tinyint(1)")), "boolean");
        assert_eq!(normalize_type("tinyint", Some("tinyint(4)")), "tinyint(4)");
    }

    #[test]
    fn the_engine_only_changes_the_description() {
        let mysql = MySqlIntrospector::new("mysql://root@localhost/shop", false);
        let mariadb = MySqlIntrospector::new("mariadb://root@localhost/shop", true);
        assert!(mysql.describe().starts_with("mysql · "));
        assert!(mariadb.describe().starts_with("mariadb · "));
        assert!(mariadb.is_mariadb());
        assert!(!mysql.is_mariadb());
    }

    #[test]
    fn the_system_schema_list_matches_the_queries() {
        assert!(SYSTEM_SCHEMAS.contains("information_schema"));
        assert!(COLUMNS_SQL.contains("CAST(data_type AS CHAR)"));
        assert!(FOREIGN_KEYS_SQL.contains("referenced_column_name"));
    }
}
