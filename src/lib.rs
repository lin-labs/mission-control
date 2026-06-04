// src/lib.rs
//
// The library target exposes the data layer and LLM layer for use by
// integration tests under `tests/`. The TUI lives in the binary target.
pub mod cli;
pub mod cmux;
pub mod llm;
pub mod mc_data;
/// Pure sidebar helpers (no binary-only deps); exposed for integration tests.
pub mod sidebar_pure;
