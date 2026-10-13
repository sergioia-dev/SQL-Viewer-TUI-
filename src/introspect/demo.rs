//! A built in schema, used by `--demo` and by the renderer's tests.
//!
//! This is a permanent fixture, not a placeholder: it is the only way to exercise
//! the whole renderer without a database, so it deliberately contains every shape
//! the viewer has to draw — a plain primary key, a foreign key, a self reference,
//! a composite primary key, a composite foreign key, a nullable column, columns
//! with defaults, and a view.

use async_trait::async_trait;

use super::{IntrospectError, SchemaIntrospector};
use crate::model::{Column, ForeignKey, KeyKind, Schema, Table};

/// Introspector for the built in schema.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoIntrospector;

impl DemoIntrospector {
    /// A demo source; no connection is ever made.
    pub fn new() -> Self {
        DemoIntrospector
    }
}

#[async_trait]
impl SchemaIntrospector for DemoIntrospector {
    async fn load(&self) -> Result<Schema, IntrospectError> {
        Ok(demo_schema())
    }

    fn describe(&self) -> String {
        "demo schema (built in)".to_string()
    }
}

/// The demo schema, already [`finalize_keys`]-ed.
///
/// [`finalize_keys`]: Schema::finalize_keys
pub fn demo_schema() -> Schema {
    let mut schema = Schema::new();

    schema.tables.push(
        Table::new("users")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("email", "text").nullable(false),
                Column::new("display_name", "text").nullable(true),
                Column::new("created_at", "timestamp")
                    .nullable(false)
                    .with_default("CURRENT_TIMESTAMP"),
            ]),
    );

    schema.tables.push(
        Table::new("categories")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("name", "varchar(64)").nullable(false),
                Column::new("parent_id", "integer"),
            ]),
    );

    schema.tables.push(
        Table::new("products")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("category_id", "integer").nullable(false),
                Column::new("sku", "varchar(32)").nullable(false),
                Column::new("price", "decimal(10,2)")
                    .nullable(false)
                    .with_default("0"),
                Column::new("description", "text"),
            ]),
    );

    schema.tables.push(
        Table::new("orders")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("user_id", "integer").nullable(false),
                Column::new("status", "varchar(16)")
                    .nullable(false)
                    .with_default("'pending'"),
                Column::new("placed_at", "timestamp")
                    .nullable(false)
                    .with_default("CURRENT_TIMESTAMP"),
            ]),
    );

    schema.tables.push(
        Table::new("order_lines")
            .with_primary_key(["order_id", "line_no"])
            .with_columns(vec![
                Column::new("order_id", "integer").nullable(false),
                Column::new("line_no", "integer").nullable(false),
                Column::new("product_id", "integer").nullable(false),
                Column::new("quantity", "integer")
                    .nullable(false)
                    .with_default("1"),
            ]),
    );

    schema.tables.push(
        Table::new("shipments")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("order_id", "integer").nullable(false),
                Column::new("line_no", "integer").nullable(false),
                Column::new("carrier", "varchar(32)").nullable(false),
                Column::new("tracking_code", "text"),
            ]),
    );

    schema.tables.push(
        Table::new("tags")
            .with_primary_key(["id"])
            .with_columns(vec![
                Column::new("id", "integer").with_key(KeyKind::Primary),
                Column::new("label", "varchar(32)").nullable(false),
            ]),
    );

    schema.tables.push(
        Table::new("product_tags")
            .with_primary_key(["product_id", "tag_id"])
            .with_columns(vec![
                Column::new("product_id", "integer").nullable(false),
                Column::new("tag_id", "integer").nullable(false),
                Column::new("added_at", "timestamp").with_default("CURRENT_TIMESTAMP"),
            ]),
    );

    schema
        .tables
        .push(Table::new("active_users").as_view().with_columns(vec![
            Column::new("id", "integer"),
            Column::new("email", "text"),
            Column::new("order_count", "integer"),
        ]));

    schema.foreign_keys.push(
        ForeignKey::new("categories", "parent_id", "categories", "id").named("fk_cat_parent"),
    );

    schema
        .foreign_keys
        .push(ForeignKey::new("products", "category_id", "categories", "id").named("fk_prod_cat"));

    schema
        .foreign_keys
        .push(ForeignKey::new("orders", "user_id", "users", "id").named("fk_order_user"));

    schema.foreign_keys.push(
        ForeignKey::new("order_lines", "product_id", "products", "id").named("fk_line_product"),
    );

    schema
        .foreign_keys
        .push(ForeignKey::new("order_lines", "order_id", "orders", "id").named("fk_line_order"));

    schema.foreign_keys.push(
        ForeignKey::composite(
            "shipments",
            vec!["order_id".into(), "line_no".into()],
            "order_lines",
            vec!["order_id".into(), "line_no".into()],
        )
        .named("fk_shipment_line"),
    );

    schema.foreign_keys.push(
        ForeignKey::new("product_tags", "product_id", "products", "id").named("fk_pt_product"),
    );

    schema
        .foreign_keys
        .push(ForeignKey::new("product_tags", "tag_id", "tags", "id").named("fk_pt_tag"));

    schema.retain_resolvable_foreign_keys();
    schema.finalize_keys();
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_schema_covers_every_shape_the_renderer_draws() {
        let schema = demo_schema();

        assert_eq!(schema.tables.len(), 9);
        assert_eq!(schema.foreign_keys.len(), 8);

        // A self reference, so the router's loop case is always exercised.
        assert!(schema
            .foreign_keys
            .iter()
            .any(|fk| fk.from_table == fk.to_table));

        // At least one composite foreign key.
        assert!(schema.foreign_keys.iter().any(ForeignKey::is_composite));

        // A nullable column and a column with a default.
        let users = schema.table("users").expect("users");
        assert!(users.column("display_name").expect("display_name").nullable);
        assert!(!users.column("email").expect("email").nullable);
        assert!(users
            .column("created_at")
            .expect("created_at")
            .default
            .is_some());

        // A view.
        assert!(schema.table("active_users").expect("active_users").is_view);
    }

    #[test]
    fn demo_keys_are_finalized() {
        let schema = demo_schema();

        let orders = schema.table("orders").expect("orders");
        assert_eq!(
            orders.column("id").expect("id").key,
            KeyKind::Primary,
            "declared primary key"
        );
        assert_eq!(
            orders.column("user_id").expect("user_id").key,
            KeyKind::Foreign,
            "foreign key column"
        );

        let lines = schema.table("order_lines").expect("order_lines");
        assert_eq!(
            lines.primary_key,
            vec!["order_id".to_string(), "line_no".to_string()],
            "composite primary key order is preserved"
        );
        assert_eq!(
            lines.column("order_id").expect("order_id").key,
            KeyKind::PrimaryForeign,
            "a join table column is both"
        );

        // The join table's composite key is derived from its foreign keys.
        let product_tags = schema.table("product_tags").expect("product_tags");
        assert_eq!(
            product_tags.primary_key,
            vec!["product_id".to_string(), "tag_id".to_string()]
        );
    }

    #[test]
    fn every_foreign_key_resolves() {
        let schema = demo_schema();
        for fk in &schema.foreign_keys {
            assert!(
                schema.table_loose(&fk.from_table).is_some(),
                "unknown child table {}",
                fk.from_table
            );
            assert!(
                schema.table_loose(&fk.to_table).is_some(),
                "unknown parent table {}",
                fk.to_table
            );
        }
    }

    #[test]
    fn the_demo_introspector_returns_the_demo_schema() {
        let loaded = futures_lite_block(&DemoIntrospector::new());
        assert_eq!(loaded, demo_schema());
        assert!(DemoIntrospector::new().describe().contains("demo"));
    }

    /// A tiny blocking executor so the async trait method can be tested without
    /// pulling a runtime into the test binary.
    fn futures_lite_block(introspector: &DemoIntrospector) -> Schema {
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        let mut future = pin!(introspector.load());
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            match Future::poll(future.as_mut(), &mut context) {
                Poll::Ready(result) => return result.expect("demo schema loads"),
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
