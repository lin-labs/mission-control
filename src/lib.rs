// src/lib.rs
//
// The library target exposes only the data layer for use by integration
// tests under `tests/`. The TUI lives in the binary target; render tests
// for trajectory_view are inline `#[cfg(test)]` unit tests inside
// src/tui/trajectory_view.rs, so the lib never needs to drag in the TUI
// module tree (which would pull in cmux/llm/session and generate dozens
// of dead-code warnings).
pub mod mc_data;
pub mod cli;
