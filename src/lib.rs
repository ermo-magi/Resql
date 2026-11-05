//! Resql: SQL-files-as-REST-endpoints microservice.
//!
//! Public API is intentionally narrow; the binary in `src/main.rs` is the
//! only supported consumer. Integration tests reach in via re-exports below.

pub mod config;
pub mod config_compat;
pub mod db;
pub mod declaration;
pub mod error;
pub mod health;
pub mod loader;
pub mod logging;
pub mod observability;
pub mod openapi;
pub mod query;
pub mod server;
