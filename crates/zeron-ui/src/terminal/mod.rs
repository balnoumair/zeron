//! Session-scoped terminal tabs over engine PTYs (RPC-backed).
//!
//! Grid paint and emulator primitives come from [`onyx_ui::terminal`].

pub mod panel;

pub use onyx_ui::terminal::{emulator, view};
