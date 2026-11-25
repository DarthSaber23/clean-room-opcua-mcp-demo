//! # readonly-mcp
//!
//! A clean-room, synthetic, **read-only** Model Context Protocol server.
//!
//! * [`jsonrpc`]: minimal JSON-RPC 2.0 message parsing and response types.
//! * [`server`]: the pure protocol handler [`server::handle`], testable without stdio.
//! * [`scope`]: RBAC [`scope::Scope`] with *monotone* narrowing (intersection only).
//! * [`catalog`]: an in-memory synthetic dataset, borrowed immutably by the session.
//! * [`oee`]: deterministic OEE arithmetic (availability x performance x quality).
//! * [`tools`]: the read-only tool definitions and their implementations.
//! * [`stdio`]: the newline-delimited transport loop used by the binary.
//!
//! There is no write path: tools receive `&Catalog`, never `&mut Catalog`.
#![forbid(unsafe_code)]

pub mod catalog;
pub mod jsonrpc;
pub mod oee;
pub mod scope;
pub mod server;
pub mod stdio;
pub mod tools;
