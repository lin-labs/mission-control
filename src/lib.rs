// src/lib.rs
pub mod mc_data;

// Re-export the modules that tui depends on so the lib target compiles.
mod cmux;
mod config;
mod llm;
mod session;

pub mod tui;
