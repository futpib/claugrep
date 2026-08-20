//! Transcript backends.
//!
//! Each backend is a self-contained `impl Source`. The program talks to a
//! [`crate::source::MultiSource`] that merges them, so adding a new backend
//! (Grok Build, …) means dropping one more module here and registering it in
//! `main` — nothing else changes.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;
