//! The frozen internal schema model.
//!
//! Every [`SchemaIntrospector`](crate::introspect::SchemaIntrospector)
//! implementation produces exactly this shape and the renderer consumes only this
//! shape: no dialect specific logic crosses this boundary.
//!
//! Composite keys are supported from day one: primary key columns are listed in
//! [`Table::primary_key`] and foreign key columns live in
//! [`ForeignKey::from_columns`] / [`ForeignKey::to_columns`] as plain
//! `Vec<String>`s, matching positionally.

use std::collections::{HashMap, HashSet};
use std::fmt;

/// How a column participates in keys.
///
/// This is a purely a display marker (it decides the `PK` / `FK` / `PK,FK` chip
/// drawn next to a column). The actual relationships live in
/// [`Schema::foreign_keys`], where composite keys are plain vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyKind {
    /// Neither part of the primary key nor part of any foreign key.
    #[default]
    None,
    /// Part of the table's primary key only.
    Primary,
    /// Part of a foreign key only.
    Foreign,
    /// Part of the primary key *and* of a foreign key (typical join table).
    PrimaryForeign,
}

impl KeyKind {
    /// The short chip rendered next to the column name, or `""` for
    /// [`KeyKind::None`].
    pub const fn marker(self) -> &'static str {
        match self {
            KeyKind::None => "",
            KeyKind::Primary => "PK",
            KeyKind::Foreign => "FK",
            KeyKind::PrimaryForeign => "PK,FK",
        }
    }

    /// Whether this column is part of the primary key.
    pub const fn is_primary(self) -> bool {
        matches!(self, KeyKind::Primary | KeyKind::PrimaryForeign)
    }

    /// Whether this column is part of some foreign key.
    pub const fn is_foreign(self) -> bool {
        matches!(self, KeyKind::Foreign | KeyKind::PrimaryForeign)
    }
}

impl From<(bool, bool)> for KeyKind {
    /// `(is_primary, is_foreign)`.
    fn from((primary, foreign): (bool, bool)) -> Self {
        match (primary, foreign) {
            (false, false) => KeyKind::None,
            (true, false) => KeyKind::Primary,
            (false, true) => KeyKind::Foreign,
            (true, true) => KeyKind::PrimaryForeign,
        }
    }
}

impl From<KeyKind> for (bool, bool) {
    fn from(kind: KeyKind) -> Self {
        (kind.is_primary(), kind.is_foreign())
    }
}

impl fmt::Display for KeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.marker())
    }
}

/// A single column of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// Normalized display string, dialect specific mapping happens inside the
    /// introspector (e.g. SQLite `VARCHAR(20)`, Postgres `character varying(20)`
    /// -> both may be shown as `varchar(20)`).
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub key: KeyKind,
}

impl Column {
    /// Convenience constructor for a plain, non-key, non-nullable column.
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Column {
            name: name.into(),
            data_type: data_type.into(),
            nullable: false,
            default: None,
            key: KeyKind::None,
        }
    }

    /// Builder: set nullability.
    pub fn nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    /// Builder: set the default expression.
    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    /// Builder: set the key kind.
    pub fn with_key(mut self, key: KeyKind) -> Self {
        self.key = key;
        self
    }

    /// The `NOT NULL` / `NULL` suffix used in wide zoom levels.
    pub fn nullability_label(&self) -> &'static str {
        if self.nullable {
            "NULL"
        } else {
            "NOT NULL"
        }
    }
}

/// One table (or view) with its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Bare table name.
    pub name: String,
    /// Namespace/owner, `None` for databases without schemas (SQLite).
    /// This is the real catalog namespace, so PostgreSQL tables carry
    /// `Some("public")` rather than `None`.
    pub schema: Option<String>,
    /// Columns in declaration order, which is also the render order.
    pub columns: Vec<Column>,
    /// Primary key columns in key order; empty when the table has no primary key.
    pub primary_key: Vec<String>,
    /// `true` for views, so the UI can label them.
    pub is_view: bool,
    /// The name the UI should show for this table: the bare name while it is
    /// unambiguous, otherwise [`Table::qualified_name`]. Derived from the whole
    /// [`Schema`] by [`Schema::finalize_keys`], so it simply defaults to the
    /// bare name until the schema has been finalised.
    label: String,
}

impl Table {
    /// A table with no columns; add columns with [`Table::with_columns`].
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Table {
            label: name.clone(),
            name,
            schema: None,
            columns: Vec::new(),
            primary_key: Vec::new(),
            is_view: false,
        }
    }

    /// Builder: set the schema/namespace.
    pub fn in_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Builder: set the columns.
    pub fn with_columns(mut self, columns: Vec<Column>) -> Self {
        self.columns = columns;
        self
    }

    /// Builder: set the primary key columns.
    pub fn with_primary_key<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.primary_key = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: mark the table as a view.
    pub fn as_view(mut self) -> Self {
        self.is_view = true;
        self
    }

    /// `schema.name` when a schema is known, otherwise the bare name.
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(schema) if !schema.is_empty() => format!("{schema}.{}", self.name),
            _ => self.name.clone(),
        }
    }

    /// The name to show in the UI: the bare name while it is unambiguous,
    /// otherwise the schema-qualified name.
    ///
    /// Derived from the whole [`Schema`] by [`Schema::finalize_keys`]. Identity
    /// lookups and foreign keys keep using [`Table::qualified_name`], so this is
    /// purely a display choice.
    pub fn display_name(&self) -> &str {
        &self.label
    }

    /// Index of `column` in [`Table::columns`].
    pub fn column_index(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == column)
    }

    /// Look a column up by name.
    pub fn column(&self, column: &str) -> Option<&Column> {
        self.column_index(column).map(|i| &self.columns[i])
    }

    /// Number of columns; the height of the table box's content area.
    pub fn width_hint(&self) -> usize {
        self.columns
            .iter()
            .map(|c| c.name.len() + c.data_type.len())
            .max()
            .unwrap_or(0)
    }
}

/// A foreign key relationship between two tables.
///
/// `from_columns` and `to_columns` are composite capable and match positionally:
/// `from_columns[i]` references `to_columns[i]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// Constraint name when the database reports one.
    pub name: Option<String>,
    /// Qualified name of the referencing table.
    pub from_table: String,
    /// Referencing (child) columns.
    pub from_columns: Vec<String>,
    /// Qualified name of the referenced table.
    pub to_table: String,
    /// Referenced (parent) columns.
    pub to_columns: Vec<String>,
}

impl ForeignKey {
    /// Build a single column foreign key.
    pub fn new(
        from_table: impl Into<String>,
        from_column: impl Into<String>,
        to_table: impl Into<String>,
        to_column: impl Into<String>,
    ) -> Self {
        ForeignKey {
            name: None,
            from_table: from_table.into(),
            from_columns: vec![from_column.into()],
            to_table: to_table.into(),
            to_columns: vec![to_column.into()],
        }
    }

    /// Build a composite foreign key from column lists.
    pub fn composite(
        from_table: impl Into<String>,
        from_columns: Vec<String>,
        to_table: impl Into<String>,
        to_columns: Vec<String>,
    ) -> Self {
        ForeignKey {
            name: None,
            from_table: from_table.into(),
            from_columns,
            to_table: to_table.into(),
            to_columns,
        }
    }

    /// Builder: set the constraint name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// `true` when the relationship spans more than one column.
    pub fn is_composite(&self) -> bool {
        self.from_columns.len() > 1 || self.to_columns.len() > 1
    }
}

/// The whole introspected schema: the single interface between the database
/// drivers and the renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl Schema {
    /// An empty schema.
    pub fn new() -> Self {
        Schema::default()
    }

    /// Look a table up by qualified name.
    pub fn table(&self, qualified_name: &str) -> Option<&Table> {
        self.tables
            .iter()
            .find(|t| t.qualified_name() == qualified_name)
    }

    /// Look a table up by qualified name, also falling back to a bare name match.
    pub fn table_loose(&self, name: &str) -> Option<&Table> {
        self.table(name).or_else(|| {
            let bare = name.rsplit('.').next().unwrap_or(name);
            self.tables
                .iter()
                .find(|t| t.name == name || t.name == bare)
        })
    }

    /// Index of a table by qualified name.
    pub fn table_index(&self, qualified_name: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.qualified_name() == qualified_name)
    }

    /// Index of a table by qualified name, falling back to a bare name match.
    pub fn table_index_loose(&self, name: &str) -> Option<usize> {
        self.table_index(name).or_else(|| {
            let bare = name.rsplit('.').next().unwrap_or(name);
            self.tables
                .iter()
                .position(|t| t.name == name || t.name == bare)
        })
    }

    /// Number of tables that are real tables (excluding views).
    pub fn table_count(&self) -> usize {
        self.tables.iter().filter(|t| !t.is_view).count()
    }

    /// Total number of columns across all tables.
    pub fn column_count(&self) -> usize {
        self.tables.iter().map(|t| t.columns.len()).sum()
    }

    /// Drop foreign keys that point at tables missing from the schema.
    ///
    /// Introspectors call this before handing the schema to the renderer so the
    /// edge router never has to deal with dangling references.
    pub fn retain_resolvable_foreign_keys(&mut self) {
        let names: HashMap<String, usize> = self
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| (t.qualified_name(), i))
            .collect();
        self.foreign_keys.retain(|fk| {
            let (Some(&from), Some(&to)) = (names.get(&fk.from_table), names.get(&fk.to_table))
            else {
                return false;
            };
            // A self-reference onto the very same columns carries no
            // information: it is how SQLite reports a single-column or
            // primary-key self reference. Routing it would draw a key
            // pointing at its own column, so drop it.
            !(from == to && fk.from_columns == fk.to_columns)
        });
    }

    /// Recompute every [`Column::key`] marker from [`Table::primary_key`] and
    /// [`Schema::foreign_keys`].
    ///
    /// Introspectors only need to record the primary key list and the foreign
    /// keys; the per column chips are derived here so all dialects agree.
    ///
    /// A table whose only key information is a foreign key and which declares no
    /// primary key of its own is treated as having a composite primary key made
    /// of those foreign key columns (the classic join table), because SQLite
    /// cannot always report such a key.
    ///
    /// It also derives [`Table::display_name`]: a table keeps its bare name
    /// unless another table in the schema shares it, in which case the
    /// schema-qualified name is used so the two can be told apart.
    pub fn finalize_keys(&mut self) {
        let index: HashMap<String, usize> = self
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| (t.qualified_name(), i))
            .collect();

        let mut is_primary: Vec<Vec<bool>> = self
            .tables
            .iter()
            .map(|t| {
                t.columns
                    .iter()
                    .map(|c| c.key.is_primary() || t.primary_key.iter().any(|k| k == &c.name))
                    .collect()
            })
            .collect();
        let mut is_foreign: Vec<Vec<bool>> = self
            .tables
            .iter()
            .map(|t| vec![false; t.columns.len()])
            .collect();

        for fk in &self.foreign_keys {
            let Some(&ti) = index.get(&fk.from_table) else {
                continue;
            };
            let table = &self.tables[ti];
            for column in &fk.from_columns {
                if let Some(ci) = table.column_index(column) {
                    is_foreign[ti][ci] = true;
                }
            }
        }

        // Join tables: a table that declares no primary key but whose leading
        // columns are all foreign keys is treated as having a composite primary
        // key made of those columns. SQLite cannot always report such a key, and
        // the union of the foreign keys is the only signal available.
        for (ti, table) in self.tables.iter().enumerate() {
            if is_primary[ti].iter().any(|p| *p) {
                continue;
            }
            let qualified = table.qualified_name();
            let mut leading = 0usize;
            for (ci, column) in table.columns.iter().enumerate() {
                let referenced = self.foreign_keys.iter().any(|fk| {
                    fk.from_table == qualified && fk.from_columns.iter().any(|f| f == &column.name)
                });
                if !referenced || ci != leading {
                    break;
                }
                leading += 1;
            }
            if leading >= 2 {
                for flag in is_primary[ti].iter_mut().take(leading) {
                    *flag = true;
                }
            }
        }
        for (ti, table) in self.tables.iter_mut().enumerate() {
            for (ci, column) in table.columns.iter_mut().enumerate() {
                column.key = KeyKind::from((is_primary[ti][ci], is_foreign[ti][ci]));
            }
            table.primary_key = table
                .columns
                .iter()
                .filter(|c| c.key.is_primary())
                .map(|c| c.name.clone())
                .collect();
        }

        // Choose the name each table shows in the UI. Unique bare names stay
        // short; only genuine collisions fall back to the schema-qualified name.
        // Identity is untouched: lookups and foreign keys still use
        // `qualified_name`.
        let mut bare_counts: HashMap<String, usize> = HashMap::new();
        for table in &self.tables {
            *bare_counts.entry(table.name.clone()).or_default() += 1;
        }
        let ambiguous: HashSet<String> = bare_counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name)
            .collect();
        for table in &mut self.tables {
            table.label = if ambiguous.contains(&table.name) {
                table.qualified_name()
            } else {
                table.name.clone()
            };
        }
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} tables, {} columns, {} foreign keys",
            self.tables.len(),
            self.column_count(),
            self.foreign_keys.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join_table_schema() -> Schema {
        let users = Table::new("users").with_columns(vec![
            Column::new("id", "integer").with_key(KeyKind::Primary),
            Column::new("email", "text"),
        ]);
        // No declared primary key at all: the composite key must be inferred.
        let memberships = Table::new("memberships").with_columns(vec![
            Column::new("user_id", "integer"),
            Column::new("team_id", "integer"),
            Column::new("role", "text"),
        ]);
        let teams = Table::new("teams")
            .with_columns(vec![Column::new("id", "integer").with_key(KeyKind::Primary)]);

        let mut schema = Schema {
            tables: vec![users, memberships, teams],
            foreign_keys: vec![
                ForeignKey::composite(
                    "memberships",
                    vec!["user_id".into()],
                    "users",
                    vec!["id".into()],
                ),
                ForeignKey::composite(
                    "memberships",
                    vec!["team_id".into()],
                    "teams",
                    vec!["id".into()],
                ),
            ],
        };
        schema.finalize_keys();
        schema
    }

    #[test]
    fn join_table_gets_composite_primary_foreign_key() {
        let schema = join_table_schema();
        let memberships = schema.table("memberships").unwrap();
        assert_eq!(
            memberships.column("user_id").unwrap().key,
            KeyKind::PrimaryForeign
        );
        assert_eq!(
            memberships.column("team_id").unwrap().key,
            KeyKind::PrimaryForeign
        );
        assert_eq!(memberships.column("role").unwrap().key, KeyKind::None);
        assert_eq!(memberships.primary_key, vec!["user_id", "team_id"]);
    }

    #[test]
    fn declared_primary_key_is_not_overridden_by_foreign_keys() {
        let mut schema = Schema {
            tables: vec![Table::new("t").with_columns(vec![
                Column::new("a", "int").with_key(KeyKind::Primary),
                Column::new("b", "int"),
            ])],
            foreign_keys: vec![ForeignKey::composite(
                "t",
                vec!["a".into(), "b".into()],
                "other",
                vec!["x".into(), "y".into()],
            )],
        };
        schema.finalize_keys();
        let table = schema.table("t").unwrap();
        assert_eq!(table.column("a").unwrap().key, KeyKind::PrimaryForeign);
        assert_eq!(table.column("b").unwrap().key, KeyKind::Foreign);
    }

    #[test]
    fn dangling_foreign_keys_are_dropped() {
        let mut schema = Schema {
            tables: vec![Table::new("a")],
            foreign_keys: vec![
                ForeignKey::new("a", "x", "ghost", "id"),
                ForeignKey::new("ghost", "x", "a", "id"),
            ],
        };
        schema.retain_resolvable_foreign_keys();
        assert!(schema.foreign_keys.is_empty());
    }

    #[test]
    fn degenerate_self_foreign_keys_are_dropped() {
        let mut schema = Schema {
            tables: vec![Table::new("categories").with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("parent_id", "integer"),
            ])],
            foreign_keys: vec![
                ForeignKey::new("categories", "parent_id", "categories", "parent_id"),
                ForeignKey::new("categories", "parent_id", "categories", "id"),
            ],
        };
        schema.retain_resolvable_foreign_keys();
        // A self-reference onto the same column carries no information, so it is
        // dropped; the genuine parent_id -> id self-FK survives.
        assert_eq!(schema.foreign_keys.len(), 1);
        assert_eq!(schema.foreign_keys[0].from_columns, vec!["parent_id"]);
        assert_eq!(schema.foreign_keys[0].to_columns, vec!["id"]);
        // As a result the primary key is never marked as a foreign key too.
        schema.finalize_keys();
        let categories = schema.table("categories").unwrap();
        assert_eq!(categories.column("id").unwrap().key, KeyKind::Primary);
        assert_eq!(
            categories.column("parent_id").unwrap().key,
            KeyKind::Foreign
        );
    }

    #[test]
    fn qualified_names() {
        assert_eq!(
            Table::new("t").in_schema("public").qualified_name(),
            "public.t"
        );
        assert_eq!(Table::new("t").qualified_name(), "t");
    }

    #[test]
    fn display_names_shorten_only_when_unambiguous() {
        let mut schema = Schema {
            tables: vec![
                Table::new("users").in_schema("public"),
                Table::new("users").in_schema("audit"),
                Table::new("orders"),
            ],
            foreign_keys: Vec::new(),
        };
        schema.finalize_keys();
        // Two `users` tables cannot both be shown as `users`, so both fall back
        // to their schema-qualified name.
        assert_eq!(
            schema.table("public.users").unwrap().display_name(),
            "public.users"
        );
        assert_eq!(
            schema.table("audit.users").unwrap().display_name(),
            "audit.users"
        );
        // A unique bare name stays short.
        assert_eq!(schema.table("orders").unwrap().display_name(), "orders");
    }

    #[test]
    fn display_name_defaults_to_the_bare_name_before_finalizing() {
        assert_eq!(
            Table::new("orders").in_schema("public").display_name(),
            "orders"
        );
    }
}
