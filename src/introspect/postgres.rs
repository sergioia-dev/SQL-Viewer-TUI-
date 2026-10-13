//! PostgreSQL introspection.
//!
//! Tables, columns and primary keys come from `information_schema`. Foreign keys
//! come from `pg_catalog` instead: `information_schema.constraint_column_usage`
//! does not preserve the column pairing of a composite key, so joining it back to
//! `key_column_usage` produces a cross product. `unnest(conkey, confkey)` pairs the
//! two column lists positionally, which is exactly the model's shape.

use std::collections::HashMap;
use std::str::FromStr;

use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::Row;

use super::{IntrospectError, SchemaIntrospector};
use crate::model::{Column, ForeignKey, Schema, Table};

/// Reads the schema of a PostgreSQL database.
#[derive(Debug, Clone)]
pub struct PostgresIntrospector {
    url: String,
}

impl PostgresIntrospector {
    /// Point at a PostgreSQL connection string. The connection is lazy.
    pub fn new(url: impl Into<String>) -> Self {
        PostgresIntrospector { url: url.into() }
    }

    /// The connection string as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Open a one-connection pool and check that it answers.
    pub async fn connect(&self) -> Result<PgPool, IntrospectError> {
        let options =
            PgConnectOptions::from_str(&self.url).map_err(|error| IntrospectError::BadUrl {
                url: super::redact(&self.url),
                reason: error.to_string(),
            })?;
        PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|error| IntrospectError::connect(&self.url, error))
    }
}

#[async_trait]
impl SchemaIntrospector for PostgresIntrospector {
    async fn load(&self) -> Result<Schema, IntrospectError> {
        load_schema(&self.connect().await?).await
    }

    fn describe(&self) -> String {
        format!("postgresql · {}", super::redact(&self.url))
    }
}

const TABLES_SQL: &str = "SELECT table_schema, table_name, table_type \
     FROM information_schema.tables \
     WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
     ORDER BY table_schema, table_name";

const COLUMNS_SQL: &str =
    "SELECT table_schema, table_name, column_name, data_type, udt_name, is_nullable, \
            column_default, character_maximum_length, numeric_precision, numeric_scale \
     FROM information_schema.columns \
     WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
     ORDER BY table_schema, table_name, ordinal_position";

const PRIMARY_KEYS_SQL: &str = "SELECT tc.table_schema, tc.table_name, kcu.column_name \
     FROM information_schema.table_constraints tc \
     JOIN information_schema.key_column_usage kcu \
       ON kcu.constraint_name = tc.constraint_name \
      AND kcu.constraint_schema = tc.constraint_schema \
     WHERE tc.constraint_type = 'PRIMARY KEY' \
       AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
     ORDER BY tc.table_schema, tc.table_name, kcu.ordinal_position";

const FOREIGN_KEYS_SQL: &str =
    "SELECT con.conname AS constraint_name, \
            src_ns.nspname AS table_schema, src.relname AS table_name, \
            src_att.attname AS column_name, \
            tgt_ns.nspname AS target_schema, tgt.relname AS target_table, \
            tgt_att.attname AS target_column, \
            k.ord AS position \
     FROM pg_catalog.pg_constraint con \
     JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS k(src_attnum, tgt_attnum, ord) ON true \
     JOIN pg_catalog.pg_class src ON src.oid = con.conrelid \
     JOIN pg_catalog.pg_namespace src_ns ON src_ns.oid = src.relnamespace \
     JOIN pg_catalog.pg_class tgt ON tgt.oid = con.confrelid \
     JOIN pg_catalog.pg_namespace tgt_ns ON tgt_ns.oid = tgt.relnamespace \
     JOIN pg_catalog.pg_attribute src_att \
       ON src_att.attrelid = src.oid AND src_att.attnum = k.src_attnum \
     JOIN pg_catalog.pg_attribute tgt_att \
       ON tgt_att.attrelid = tgt.oid AND tgt_att.attnum = k.tgt_attnum \
     WHERE con.contype = 'f' \
       AND src_ns.nspname NOT IN ('pg_catalog', 'information_schema') \
     ORDER BY con.conname, k.ord";

/// Read the whole schema of the database behind `pool`.
pub async fn load_schema(pool: &PgPool) -> Result<Schema, IntrospectError> {
    let mut schema = Schema::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();

    for row in query(pool, TABLES_SQL, "list tables").await? {
        let table_schema = text(&row, "table_schema")?.unwrap_or_default();
        let table_name = text(&row, "table_name")?.unwrap_or_default();
        let table_type = text(&row, "table_type")?.unwrap_or_default();
        let mut table = Table::new(table_name.as_str()).in_schema(table_schema.as_str());
        if table_type.eq_ignore_ascii_case("VIEW") {
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
        let udt_name = text(&row, "udt_name")?;
        let display = normalize_type(
            &data_type,
            udt_name.as_deref(),
            integer(&row, "character_maximum_length")?,
            integer(&row, "numeric_precision")?,
            integer(&row, "numeric_scale")?,
        );
        let mut column = Column::new(name, display).nullable(
            !text(&row, "is_nullable")?
                .unwrap_or_default()
                .eq_ignore_ascii_case("YES"),
        );
        if let Some(default) = text(&row, "column_default")?.filter(|value| !value.is_empty()) {
            column = column.with_default(default);
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

/// How a PostgreSQL type is shown in a box.
///
/// `information_schema.data_type` spells types in SQL standard words ("character
/// varying", "timestamp without time zone"); we shorten the ones that have a
/// familiar short name and fold the length/precision back in. `udt_name` is used when
/// the standard name hides the real type, as it does for enums and arrays.
pub fn normalize_type(
    data_type: &str,
    udt_name: Option<&str>,
    character_length: Option<i32>,
    precision: Option<i32>,
    scale: Option<i32>,
) -> String {
    if data_type.eq_ignore_ascii_case("ARRAY") {
        let inner = udt_name.unwrap_or_default().trim_start_matches('_');
        return if inner.is_empty() {
            "array".to_string()
        } else {
            format!("{inner}[]")
        };
    }

    let base = match data_type {
        "character varying" => "varchar",
        "character" => "char",
        "USER-DEFINED" | "domain" => udt_name.unwrap_or(data_type),
        "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "time without time zone" => "time",
        "time with time zone" => "timetz",
        "bit varying" => "varbit",
        other => other,
    };

    match base {
        "varchar" | "char" | "bit" | "varbit" => match character_length {
            Some(width) if width > 0 => format!("{base}({width})"),
            _ => base.to_string(),
        },
        "numeric" | "decimal" => match (precision, scale) {
            (Some(precision), Some(scale)) => format!("numeric({precision},{scale})"),
            _ => "numeric".to_string(),
        },
        other => other.to_string(),
    }
}

async fn query(pool: &PgPool, sql: &str, context: &str) -> Result<Vec<PgRow>, IntrospectError> {
    sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| IntrospectError::query(context, error))
}

fn lookup(
    index_of: &HashMap<(String, String), usize>,
    row: &PgRow,
) -> Result<Option<usize>, IntrospectError> {
    let table_schema = text(row, "table_schema")?.unwrap_or_default();
    let table_name = text(row, "table_name")?.unwrap_or_default();
    Ok(index_of.get(&(table_schema, table_name)).copied())
}

fn text(row: &PgRow, column: &str) -> Result<Option<String>, IntrospectError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|error| IntrospectError::query(format!("postgres column {column}"), error))
}

fn integer(row: &PgRow, column: &str) -> Result<Option<i32>, IntrospectError> {
    row.try_get::<Option<i32>, _>(column)
        .map_err(|error| IntrospectError::query(format!("postgres column {column}"), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_precision_are_folded_into_the_display_type() {
        assert_eq!(
            normalize_type("character varying", Some("varchar"), Some(255), None, None),
            "varchar(255)"
        );
        assert_eq!(
            normalize_type("character varying", Some("varchar"), None, None, None),
            "varchar"
        );
        assert_eq!(
            normalize_type("numeric", Some("numeric"), None, Some(10), Some(2)),
            "numeric(10,2)"
        );
        assert_eq!(
            normalize_type("numeric", Some("numeric"), None, None, None),
            "numeric"
        );
    }

    #[test]
    fn standard_type_names_get_their_short_form() {
        assert_eq!(
            normalize_type("timestamp without time zone", None, None, None, None),
            "timestamp"
        );
        assert_eq!(
            normalize_type("timestamp with time zone", None, None, None, None),
            "timestamptz"
        );
        assert_eq!(
            normalize_type("time without time zone", None, None, None, None),
            "time"
        );
        assert_eq!(normalize_type("integer", None, None, None, None), "integer");
        assert_eq!(normalize_type("boolean", None, None, None, None), "boolean");
    }

    #[test]
    fn enums_and_arrays_keep_their_real_type_name() {
        assert_eq!(
            normalize_type("USER-DEFINED", Some("order_status"), None, None, None),
            "order_status"
        );
        assert_eq!(
            normalize_type("ARRAY", Some("_int4"), None, None, None),
            "int4[]"
        );
    }

    #[test]
    fn qualified_names_only_prefix_a_real_schema() {
        assert_eq!(qualified("public", "users"), "public.users");
        assert_eq!(qualified("", "users"), "users");
    }
}
