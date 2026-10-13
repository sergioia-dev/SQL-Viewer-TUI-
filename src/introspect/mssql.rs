//! SQL Server introspection over the TDS protocol (`tiberius`).
//!
//! Unlike the sqlx drivers, tiberius takes an ADO connection string rather than a
//! URL, so a URL such as `mssql://sa:secret@localhost:1433/shop` is translated
//! first — see [`to_ado_string`]. The scheme is stripped before anything else, so a
//! hybrid `sqlserver://host:port;user=…;password=…` form keeps its ADO tail, while a
//! string with no recognised scheme is passed through untouched.
//!
//! The system catalog views (`sys.tables`, `sys.columns`, `sys.foreign_keys`) are the
//! only place SQL Server exposes composite key pairing.
//!
//! Every numeric column is `CAST(... AS INT)` so the row reader only ever has to
//! decode integers and wide strings, and flag columns are turned into `'YES'`/`'NO'`
//! text for the same reason.

use async_trait::async_trait;
use tiberius::{Client, Config, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{IntrospectError, SchemaIntrospector};
use crate::model::{Column, ForeignKey, Schema, Table};

const OBJECTS_SQL: &str = "SELECT s.name AS schema_name, o.name AS object_name, \
            CASE WHEN o.type = 'V' THEN 1 ELSE 0 END AS is_view \
     FROM sys.objects o \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
     ORDER BY s.name, o.name";

const COLUMNS_SQL: &str =
    "SELECT s.name AS schema_name, o.name AS object_name, c.name AS column_name, \
            ty.name AS type_name, \
            CAST(c.max_length AS INT) AS max_length, \
            CAST(c.precision AS INT) AS precision, \
            CAST(c.scale AS INT) AS scale, \
            CASE WHEN c.is_nullable = 1 THEN 'YES' ELSE 'NO' END AS is_nullable, \
            dc.definition AS column_default \
     FROM sys.columns c \
     JOIN sys.objects o ON o.object_id = c.object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
     LEFT JOIN sys.default_constraints dc \
       ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
     WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
     ORDER BY s.name, o.name, c.column_id";

const PRIMARY_KEYS_SQL: &str =
    "SELECT s.name AS schema_name, o.name AS object_name, c.name AS column_name \
     FROM sys.indexes i \
     JOIN sys.index_columns ic \
       ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
     JOIN sys.columns c \
       ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
     JOIN sys.objects o ON o.object_id = i.object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     WHERE i.is_primary_key = 1 AND o.is_ms_shipped = 0 \
     ORDER BY s.name, o.name, ic.key_ordinal";

const FOREIGN_KEYS_SQL: &str = "SELECT fk.name AS constraint_name, s.name AS schema_name, \
            o.name AS object_name, c.name AS column_name, \
            ts.name AS target_schema, tgt.name AS target_object, \
            tc.name AS target_column \
     FROM sys.foreign_keys fk \
     JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
     JOIN sys.objects o ON o.object_id = fkc.parent_object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     JOIN sys.columns c \
       ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id \
     JOIN sys.objects tgt ON tgt.object_id = fkc.referenced_object_id \
     JOIN sys.schemas ts ON ts.schema_id = tgt.schema_id \
     JOIN sys.columns tc \
       ON tc.object_id = fkc.referenced_object_id \
      AND tc.column_id = fkc.referenced_column_id \
     WHERE o.is_ms_shipped = 0 \
     ORDER BY fk.name, fkc.constraint_column_id";

/// Reads the schema of a SQL Server database.
#[derive(Debug, Clone)]
pub struct MsSqlIntrospector {
    config: Config,
    url: String,
}

impl MsSqlIntrospector {
    /// Parse a SQL Server connection URL (or a ready-made ADO string).
    ///
    /// Fallible because the connection string is parsed eagerly: a typo should be
    /// reported before the terminal switches to the alternate screen.
    pub fn new(url: &str) -> Result<Self, IntrospectError> {
        let ado = to_ado_string(url)?;
        let config = Config::from_ado_string(&ado).map_err(|error| IntrospectError::BadUrl {
            url: super::redact(url),
            reason: error.to_string(),
        })?;
        Ok(MsSqlIntrospector {
            config,
            url: url.to_string(),
        })
    }

    /// The connection string as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Open a client, negotiating the TDS pre-login handshake.
    pub async fn connect(&self) -> Result<Client<Compat<TcpStream>>, IntrospectError> {
        let address = self.config.get_addr();
        let tcp = TcpStream::connect(&address)
            .await
            .map_err(|error| IntrospectError::connect(&self.url, error))?;
        tcp.set_nodelay(true)
            .map_err(|error| IntrospectError::connect(&self.url, error))?;
        Client::connect(self.config.clone(), tcp.compat_write())
            .await
            .map_err(|error| IntrospectError::connect(&self.url, error))
    }
}

#[async_trait]
impl SchemaIntrospector for MsSqlIntrospector {
    async fn load(&self) -> Result<Schema, IntrospectError> {
        let mut client = self.connect().await?;
        load_schema(&mut client).await
    }

    fn describe(&self) -> String {
        let address = self.config.get_addr();
        format!("mssql · {address}")
    }
}

/// Translate `mssql://user:pass@host:port/db` into an ADO connection string.
///
/// The scheme is stripped first, so a hybrid string that carries ADO key/value
/// pairs after the authority — for example
/// `sqlserver://localhost:1433;user=sa;password=x` — keeps that tail as-is.
/// Percent escapes in the URL userinfo are decoded; the ADO tail is not.
///
/// A `?query` on the database path is emitted as ADO `key=value;` pairs, so
/// `.../db?encrypt=optional&trustServerCertificate=true` reaches tiberius as
/// connection options instead of being swallowed by the `database=` value.
///
/// Only a string with no recognised scheme is taken to be a ready-made ADO
/// connection string and passed through untouched. This exists because the driver
/// list works on URLs while tiberius works on ADO strings.
pub fn to_ado_string(url: &str) -> Result<String, IntrospectError> {
    let bad = |reason: &str| IntrospectError::BadUrl {
        url: super::redact(url),
        reason: reason.to_string(),
    };

    let Some(rest) = [
        "mssql://",
        "sqlserver://",
        "tds://",
        "mssql:",
        "sqlserver:",
        "tds:",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix)) else {
        // No scheme: an ADO/ODBC configuration string is passed through untouched.
        if url.contains('=') {
            return Ok(url.to_string());
        }
        return Err(bad("expected an mssql:// URL or an ADO connection string"));
    };

    // A hybrid `scheme://host:port;key=value;...` string keeps its ADO tail verbatim.
    let (authority, ado_tail) = match rest.split_once(';') {
        Some((authority, ado_tail)) => (authority, Some(ado_tail)),
        None => (rest, None),
    };
    let (authority, database) = match authority.split_once('/') {
        Some((authority, database)) => (authority, Some(database)),
        None => (authority, None),
    };
    if authority.is_empty() {
        return Err(bad("the connection URL has no host"));
    }

    // Connection options arrive as a `?query` on the database path (dadbod emits
    // `encrypt` and `trustServerCertificate` that way). Split it off here, or it
    // would be glued onto the `database=` value and never reach tiberius.
    let (database, query) = match database {
        Some(database) => match database.split_once('?') {
            Some((database, query)) => (Some(database), Some(query)),
            None => (Some(database), None),
        },
        None => (None, None),
    };

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (hostport, None),
    };
    if host.is_empty() {
        return Err(bad("the connection URL has no host"));
    }

    let mut ado = String::new();
    match port {
        Some(port) => ado.push_str(&format!("server=tcp:{host},{port};")),
        None => ado.push_str(&format!("server=tcp:{host},1433;")),
    }
    if let Some(userinfo) = userinfo {
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (userinfo, None),
        };
        ado.push_str(&format!("user={};", percent_decode(user)));
        if let Some(password) = password {
            ado.push_str(&format!("password={};", percent_decode(password)));
        }
    }
    let database = database.unwrap_or_default().trim_matches('/');
    if !database.is_empty() {
        ado.push_str(&format!("database={database};"));
    }
    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            match pair.split_once('=') {
                Some((key, value)) => {
                    ado.push_str(&format!(
                        "{}={};",
                        percent_decode(key),
                        percent_decode(value)
                    ));
                }
                None => ado.push_str(&format!("{};", percent_decode(pair))),
            }
        }
    }
    if let Some(ado_tail) = ado_tail {
        let ado_tail = ado_tail.trim().trim_end_matches(';');
        if !ado_tail.is_empty() {
            ado.push_str(ado_tail);
            ado.push(';');
        }
    }
    Ok(ado)
}

/// Decode `%XX` escapes, leaving anything that is not a valid escape untouched.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The value of a single hexadecimal digit, or `None` when it is not one.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Read the whole schema of the database behind `client`.
pub async fn load_schema(
    client: &mut Client<Compat<TcpStream>>,
) -> Result<Schema, IntrospectError> {
    use std::collections::HashMap;

    let mut schema = Schema::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();

    for row in query(client, OBJECTS_SQL, "list tables").await? {
        let table_schema = text(&row, "schema_name")?.unwrap_or_default();
        let object_name = text(&row, "object_name")?.unwrap_or_default();
        let is_view = integer(&row, "is_view")?.unwrap_or_default() == 1;
        let mut table = Table::new(object_name.as_str()).in_schema(table_schema.as_str());
        if is_view {
            table = table.as_view();
        }
        index_of.insert((table_schema, object_name), schema.tables.len());
        schema.tables.push(table);
    }

    for row in query(client, COLUMNS_SQL, "list columns").await? {
        let Some(index) = lookup(&index_of, &row)? else {
            continue;
        };
        let name = text(&row, "column_name")?.unwrap_or_default();
        let type_name = text(&row, "type_name")?.unwrap_or_default();
        let display = normalize_type(
            &type_name,
            integer(&row, "max_length")?,
            integer(&row, "precision")?,
            integer(&row, "scale")?,
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

    for row in query(client, PRIMARY_KEYS_SQL, "list primary keys").await? {
        let Some(index) = lookup(&index_of, &row)? else {
            continue;
        };
        let column = text(&row, "column_name")?.unwrap_or_default();
        schema.tables[index].primary_key.push(column);
    }

    let mut pending: Option<PendingForeignKey> = None;
    for row in query(client, FOREIGN_KEYS_SQL, "list foreign keys").await? {
        let name = text(&row, "constraint_name")?.unwrap_or_default();
        let from_schema = text(&row, "schema_name")?.unwrap_or_default();
        let from_table = text(&row, "object_name")?.unwrap_or_default();
        let from_column = text(&row, "column_name")?.unwrap_or_default();
        let to_schema = text(&row, "target_schema")?.unwrap_or_default();
        let to_table = text(&row, "target_object")?.unwrap_or_default();
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

/// How a SQL Server type is shown in a box.
///
/// `sys.types.name` is the bare name ("nvarchar", "decimal"), so the length or
/// precision has to be folded back in. `max_length` is reported in bytes, which means
/// a Unicode type carries double; `-1` means `max`.
pub fn normalize_type(
    type_name: &str,
    max_length: Option<i32>,
    precision: Option<i32>,
    scale: Option<i32>,
) -> String {
    let name = type_name.trim();
    match name.to_ascii_lowercase().as_str() {
        "nvarchar" | "nchar" => match max_length {
            Some(-1) => format!("{name}(max)"),
            Some(bytes) => format!("{name}({})", bytes.max(0) / 2),
            None => name.to_string(),
        },
        "varchar" | "char" | "varbinary" | "binary" => match max_length {
            Some(-1) => format!("{name}(max)"),
            Some(bytes) => format!("{name}({bytes})"),
            None => name.to_string(),
        },
        "decimal" | "numeric" => match (precision, scale) {
            (Some(precision), Some(scale)) => format!("{name}({precision},{scale})"),
            (Some(precision), None) => format!("{name}({precision})"),
            _ => name.to_string(),
        },
        "datetime2" | "datetimeoffset" | "time" => match scale {
            Some(scale) if scale > 0 => format!("{name}({scale})"),
            _ => name.to_string(),
        },
        _ => name.to_string(),
    }
}

async fn query(
    client: &mut Client<Compat<TcpStream>>,
    sql: &str,
    context: &str,
) -> Result<Vec<Row>, IntrospectError> {
    client
        .simple_query(sql)
        .await
        .map_err(|error| IntrospectError::query(context, error))?
        .into_first_result()
        .await
        .map_err(|error| IntrospectError::query(context, error))
}

fn lookup(
    index_of: &std::collections::HashMap<(String, String), usize>,
    row: &Row,
) -> Result<Option<usize>, IntrospectError> {
    let table_schema = text(row, "schema_name")?.unwrap_or_default();
    let object_name = text(row, "object_name")?.unwrap_or_default();
    Ok(index_of.get(&(table_schema, object_name)).copied())
}

fn text(row: &Row, column: &str) -> Result<Option<String>, IntrospectError> {
    row.try_get::<&str, _>(column)
        .map(|value| value.map(|value| value.to_string()))
        .map_err(|error| IntrospectError::query(format!("mssql column {column}"), error))
}

fn integer(row: &Row, column: &str) -> Result<Option<i32>, IntrospectError> {
    row.try_get::<i32, _>(column)
        .map_err(|error| IntrospectError::query(format!("mssql column {column}"), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_becomes_an_ado_string() {
        assert_eq!(
            to_ado_string("mssql://sa:secret@localhost:1433/shop").unwrap(),
            "server=tcp:localhost,1433;user=sa;password=secret;database=shop;"
        );
    }

    #[test]
    fn the_default_port_and_missing_credentials_are_handled() {
        assert_eq!(
            to_ado_string("mssql://localhost/shop").unwrap(),
            "server=tcp:localhost,1433;database=shop;"
        );
        assert_eq!(
            to_ado_string("mssql://localhost").unwrap(),
            "server=tcp:localhost,1433;"
        );
    }

    #[test]
    fn an_existing_ado_string_is_passed_through() {
        let ado = "server=tcp:db.example.com,1433;user=sa;password=x;TrustServerCertificate=true";
        assert_eq!(to_ado_string(ado).unwrap(), ado);
    }

    #[test]
    fn a_string_that_is_neither_a_url_nor_an_ado_string_is_rejected() {
        assert!(to_ado_string("localhost:1433").is_err());
        assert!(to_ado_string("mssql://").is_err());
    }

    #[test]
    fn a_hybrid_url_keeps_its_ado_tail() {
        assert_eq!(
            to_ado_string("sqlserver://localhost:1433;user=sa;password=YourStrong%21Passw0rd")
                .unwrap(),
            "server=tcp:localhost,1433;user=sa;password=YourStrong%21Passw0rd;"
        );
    }

    #[test]
    fn percent_escapes_in_the_userinfo_are_decoded() {
        assert_eq!(
            to_ado_string("mssql://sa:YourStrong%21Passw0rd@localhost/shop").unwrap(),
            "server=tcp:localhost,1433;user=sa;password=YourStrong!Passw0rd;database=shop;"
        );
        // An escape that is not valid hexadecimal is left as-is.
        assert_eq!(
            to_ado_string("mssql://sa:p%zz@localhost").unwrap(),
            "server=tcp:localhost,1433;user=sa;password=p%zz;"
        );
    }

    #[test]
    fn the_tds_scheme_is_accepted() {
        assert_eq!(
            to_ado_string("tds://localhost/db").unwrap(),
            "server=tcp:localhost,1433;database=db;"
        );
    }

    #[test]
    fn query_params_become_ado_tail() {
        assert_eq!(
            to_ado_string(
                "sqlserver://sa:secret@localhost:1433/child?encrypt=optional&trustServerCertificate=true"
            )
            .unwrap(),
            "server=tcp:localhost,1433;user=sa;password=secret;database=child;encrypt=optional;trustServerCertificate=true;"
        );
    }

    #[test]
    fn query_params_are_percent_decoded() {
        assert_eq!(
            to_ado_string("mssql://sa@localhost/db?trustServerCertificate=true&encrypt=optional")
                .unwrap(),
            "server=tcp:localhost,1433;user=sa;database=db;trustServerCertificate=true;encrypt=optional;"
        );
    }

    #[test]
    fn unicode_lengths_are_reported_in_characters() {
        assert_eq!(
            normalize_type("nvarchar", Some(200), None, None),
            "nvarchar(100)"
        );
        assert_eq!(normalize_type("nchar", Some(20), None, None), "nchar(10)");
        assert_eq!(
            normalize_type("nvarchar", Some(-1), None, None),
            "nvarchar(max)"
        );
    }

    #[test]
    fn byte_lengths_and_precision_are_kept_as_reported() {
        assert_eq!(
            normalize_type("varchar", Some(64), None, None),
            "varchar(64)"
        );
        assert_eq!(
            normalize_type("varbinary", Some(-1), None, None),
            "varbinary(max)"
        );
        assert_eq!(
            normalize_type("decimal", None, Some(10), Some(2)),
            "decimal(10,2)"
        );
        assert_eq!(
            normalize_type("datetime2", None, None, Some(3)),
            "datetime2(3)"
        );
    }

    #[test]
    fn plain_types_are_left_alone() {
        assert_eq!(normalize_type("int", Some(4), Some(10), Some(0)), "int");
        assert_eq!(normalize_type("bit", Some(1), None, None), "bit");
        assert_eq!(
            normalize_type("uniqueidentifier", Some(16), None, None),
            "uniqueidentifier"
        );
    }
}
