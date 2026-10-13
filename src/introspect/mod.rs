//! Schema introspection: the only place that talks to a database.
//!
//! Every driver implements [`SchemaIntrospector`] and returns the frozen
//! [`Schema`] model, so nothing downstream knows which database it is looking at.
//! All queries go through sqlx's runtime API — never the `query!` macros — because
//! the databases being inspected are arbitrary and cannot be known at build time.

pub mod demo;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

use std::fmt;

use async_trait::async_trait;

use crate::model::Schema;

/// Everything that can go wrong on the way to a [`Schema`].
///
/// Connection strings are redacted before they are stored, so an error can be
/// printed or logged without leaking a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntrospectError {
    /// The URL scheme does not name a supported database.
    UnknownDriver { scheme: String },
    /// The connection string is missing pieces we cannot guess.
    BadUrl { url: String, reason: String },
    /// The server could not be reached or refused the credentials.
    Connect { url: String, reason: String },
    /// A query was rejected or returned something unexpected.
    Query { context: String, reason: String },
}

impl IntrospectError {
    /// A connection failure, with the password stripped from `url`.
    pub fn connect(url: impl AsRef<str>, reason: impl fmt::Display) -> Self {
        IntrospectError::Connect {
            url: redact(url.as_ref()),
            reason: reason.to_string(),
        }
    }

    /// A query failure.
    pub fn query(context: impl Into<String>, reason: impl fmt::Display) -> Self {
        IntrospectError::Query {
            context: context.into(),
            reason: reason.to_string(),
        }
    }

    /// A one line suggestion to print under the error, when we have one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            IntrospectError::UnknownDriver { .. } => Some(
                "Use a URL such as sqlite:///path/to.db, postgres://…, mysql://…, mariadb://… or mssql://…; \
                 --db overrides an ambiguous scheme.",
            ),
            IntrospectError::BadUrl { .. } => Some(
                "Expected something like sqlite:///data/app.db, postgres://user:pass@host/db, \
                 mysql://user@host:3306/db, mssql://user:pass@host:1433/db.",
            ),
            IntrospectError::Connect { .. } => Some(
                "Check the host, port, credentials and that the server is running.",
            ),
            IntrospectError::Query { .. } => {
                Some("The account may lack permission to read the catalog; try a read-only admin role.")
            }
        }
    }
}

impl fmt::Display for IntrospectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntrospectError::UnknownDriver { scheme } => {
                write!(f, "unsupported connection scheme {scheme:?}")
            }
            IntrospectError::BadUrl { url, reason } => {
                write!(f, "cannot read connection string {url:?}: {reason}")
            }
            IntrospectError::Connect { url, reason } => {
                write!(f, "cannot connect to {url}: {reason}")
            }
            IntrospectError::Query { context, reason } => {
                write!(f, "query failed ({context}): {reason}")
            }
        }
    }
}

impl std::error::Error for IntrospectError {}

/// Replace the password in a connection string with `***`.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return url.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

/// Which dialect a connection string speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverKind {
    Sqlite,
    Postgres,
    Mysql,
    MariaDb,
    MsSql,
}

impl DriverKind {
    /// Every supported driver, in documentation order.
    pub const ALL: [DriverKind; 5] = [
        DriverKind::Sqlite,
        DriverKind::Postgres,
        DriverKind::Mysql,
        DriverKind::MariaDb,
        DriverKind::MsSql,
    ];

    /// The name used by `--db` and in messages.
    pub const fn slug(self) -> &'static str {
        match self {
            DriverKind::Sqlite => "sqlite",
            DriverKind::Postgres => "postgres",
            DriverKind::Mysql => "mysql",
            DriverKind::MariaDb => "mariadb",
            DriverKind::MsSql => "mssql",
        }
    }

    /// Parse a `--db` value. Accepts the obvious aliases.
    pub fn parse(value: &str) -> Option<DriverKind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" | "sql" => Some(DriverKind::Sqlite),
            "postgres" | "postgresql" | "pg" => Some(DriverKind::Postgres),
            "mysql" => Some(DriverKind::Mysql),
            "mariadb" | "maria" => Some(DriverKind::MariaDb),
            "mssql" | "sqlserver" | "sql-server" | "tds" => Some(DriverKind::MsSql),
            _ => None,
        }
    }

    /// The driver implied by a connection string's scheme.
    pub fn from_url(url: &str) -> Result<DriverKind, IntrospectError> {
        let scheme = url
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .or_else(|| url.split_once(':').map(|(scheme, _)| scheme))
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match scheme.as_str() {
            "sqlite" | "sqlite3" | "file" => Ok(DriverKind::Sqlite),
            // No scheme at all — a bare path such as `app.db` — is SQLite.
            "" => Ok(DriverKind::Sqlite),
            // A single letter and no `://` is a Windows drive letter, not a scheme.
            other if other.len() == 1 && !url.contains("://") => Ok(DriverKind::Sqlite),
            "postgres" | "postgresql" | "pg" => Ok(DriverKind::Postgres),
            "mysql" => Ok(DriverKind::Mysql),
            "mariadb" => Ok(DriverKind::MariaDb),
            "mssql" | "sqlserver" | "tds" => Ok(DriverKind::MsSql),
            _ => Err(IntrospectError::UnknownDriver { scheme }),
        }
    }
}

impl fmt::Display for DriverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// A resolved connection: which driver, and where to point it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub driver: DriverKind,
    pub url: String,
}

impl Target {
    /// Work out the driver from the URL, letting `override_driver` win.
    pub fn resolve(url: &str, override_driver: Option<&str>) -> Result<Target, IntrospectError> {
        if url.trim().is_empty() {
            return Err(IntrospectError::BadUrl {
                url: String::new(),
                reason: "empty connection string".into(),
            });
        }
        let driver = match override_driver {
            Some(raw) => DriverKind::parse(raw).ok_or_else(|| IntrospectError::BadUrl {
                url: redact(url),
                reason: format!("unknown driver {raw:?}"),
            })?,
            None => DriverKind::from_url(url)?,
        };
        Ok(Target {
            driver,
            url: url.to_string(),
        })
    }

    /// Connection string with any password replaced by `***`.
    pub fn display_url(&self) -> String {
        redact(&self.url)
    }
}

/// One database flavour's schema reader.
///
/// Implementations own their connection; [`load`](SchemaIntrospector::load) is
/// called once at startup and must return a fully [`finalize_keys`]-ed schema.
///
/// [`finalize_keys`]: Schema::finalize_keys
#[async_trait]
pub trait SchemaIntrospector: Send + Sync {
    /// Read the whole schema.
    async fn load(&self) -> Result<Schema, IntrospectError>;

    /// A short label for the status bar, e.g. `sqlite · app.db`.
    fn describe(&self) -> String;
}

/// Build the introspector for a target.
pub fn open(target: &Target) -> Result<Box<dyn SchemaIntrospector>, IntrospectError> {
    Ok(match target.driver {
        DriverKind::Sqlite => Box::new(sqlite::SqliteIntrospector::new(&target.url)),
        DriverKind::Postgres => Box::new(postgres::PostgresIntrospector::new(&target.url)),
        DriverKind::Mysql | DriverKind::MariaDb => Box::new(mysql::MySqlIntrospector::new(
            &target.url,
            target.driver == DriverKind::MariaDb,
        )),
        DriverKind::MsSql => Box::new(mssql::MsSqlIntrospector::new(&target.url)?),
    })
}

/// Connect to `target` and read its schema in one step.
pub async fn introspect(target: &Target) -> Result<Schema, IntrospectError> {
    open(target)?.load().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_map_to_drivers() {
        assert_eq!(
            DriverKind::from_url("sqlite:///tmp/a.db"),
            Ok(DriverKind::Sqlite)
        );
        assert_eq!(
            DriverKind::from_url("sqlite::memory:"),
            Ok(DriverKind::Sqlite)
        );
        assert_eq!(
            DriverKind::from_url("file:/tmp/a.db"),
            Ok(DriverKind::Sqlite)
        );
        assert_eq!(
            DriverKind::from_url("postgres://user@host/db"),
            Ok(DriverKind::Postgres)
        );
        assert_eq!(
            DriverKind::from_url("postgresql://host/db"),
            Ok(DriverKind::Postgres)
        );
        assert_eq!(
            DriverKind::from_url("mysql://host/db"),
            Ok(DriverKind::Mysql)
        );
        assert_eq!(
            DriverKind::from_url("mariadb://host/db"),
            Ok(DriverKind::MariaDb)
        );
        assert_eq!(
            DriverKind::from_url("mssql://host/db"),
            Ok(DriverKind::MsSql)
        );
        assert_eq!(
            DriverKind::from_url("sqlserver://host/db"),
            Ok(DriverKind::MsSql)
        );
        assert!(matches!(
            DriverKind::from_url("oracle://host/db"),
            Err(IntrospectError::UnknownDriver { .. })
        ));
    }

    #[test]
    fn driver_aliases_parse() {
        assert_eq!(DriverKind::parse("PostgreSQL"), Some(DriverKind::Postgres));
        assert_eq!(DriverKind::parse("pg"), Some(DriverKind::Postgres));
        assert_eq!(DriverKind::parse("sqlserver"), Some(DriverKind::MsSql));
        assert_eq!(DriverKind::parse("sqlite3"), Some(DriverKind::Sqlite));
        assert_eq!(DriverKind::parse("oracle"), None);
    }

    #[test]
    fn an_override_beats_an_ambiguous_scheme() {
        let target = Target::resolve("mysql://host/db", Some("mariadb")).expect("resolved");
        assert_eq!(target.driver, DriverKind::MariaDb);
    }

    #[test]
    fn resolve_rejects_empty_and_unknown_input() {
        assert!(Target::resolve("", None).is_err());
        assert!(Target::resolve("oracle://host/db", None).is_err());
        assert!(Target::resolve("mysql://host/db", Some("oracle")).is_err());
    }

    #[test]
    fn passwords_are_redacted() {
        assert_eq!(
            redact("postgres://alice:s3cret@db.example:5432/app"),
            "postgres://alice:***@db.example:5432/app"
        );
        assert_eq!(
            redact("mysql://root@localhost/app"),
            "mysql://root@localhost/app"
        );
        assert_eq!(redact("sqlite:///tmp/a.db"), "sqlite:///tmp/a.db");
    }

    #[test]
    fn error_messages_do_not_leak_passwords() {
        let error = IntrospectError::connect("postgres://alice:s3cret@host/db", "timed out");
        let text = error.to_string();
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
    }

    #[test]
    fn every_driver_has_a_distinct_slug() {
        let mut slugs: Vec<&str> = DriverKind::ALL.iter().map(|d| d.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), DriverKind::ALL.len());
        for driver in DriverKind::ALL {
            assert_eq!(DriverKind::parse(driver.slug()), Some(driver));
        }
    }
}
