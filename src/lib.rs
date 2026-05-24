// src/lib.rs
//
// The library target exposes the data layer and LLM layer for use by
// integration tests under `tests/`. The TUI lives in the binary target.
pub mod mc_data;
pub mod cli;
pub mod llm;
/// Pure sidebar helpers (no binary-only deps); exposed for integration tests.
pub mod sidebar_pure;
