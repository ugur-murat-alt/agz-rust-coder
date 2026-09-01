#![forbid(unsafe_code)]

pub mod cache;
pub mod config;
pub mod diagnostics;
pub mod docs;
pub mod gate;
pub mod knowledge;
pub mod lsp;
pub mod process;
pub mod server;
pub mod telemetry;
pub mod tools;
pub mod workspace;

pub use config::{CliOptions, Config, ConfigError};
pub use server::{AppState, RustCoderServer, ShutdownError};
