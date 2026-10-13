//! `sqlv` — a keyboard-driven terminal ER diagram viewer for live SQL databases.
//!
//! The crate is split along one hard boundary: every introspector
//! ([`introspect`]) produces the frozen [`model::Schema`], and everything else
//! ([`layout`], [`table_view`], [`edges`], [`app`]) consumes only that model.
//! No dialect specific logic crosses the boundary.

pub mod app;
pub mod canvas;
pub mod edges;
pub mod introspect;
pub mod layout;
pub mod model;
pub mod table_view;
pub mod zoom;
