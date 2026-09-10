#![forbid(unsafe_code)]

pub mod cache;
pub mod change;
pub mod config;
pub mod context;
pub mod diagnostics;
pub mod docs;
pub mod gate;
pub mod knowledge;
pub mod lsp;
pub mod process;
pub mod repair;
pub mod server;
pub mod telemetry;
pub mod tools;
pub mod work;
pub mod workspace;

pub use config::{CliOptions, Config, ConfigError};
pub use server::{AppState, RustCoderServer, ShutdownError};
