#![forbid(unsafe_code)]

use std::{error::Error, time::Duration};

use agz_rust_coder::{Config, ConfigError, RustCoderServer};
use clap::error::ErrorKind;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let config = match Config::load_from(std::env::args_os()) {
        Ok(config) => config,
        Err(ConfigError::Cli(error))
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let server = RustCoderServer::new(config)?;
    let state = server.state().clone();
    let service = server.serve(stdio()).await?;
    let transport_cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);

    let transport_result: Result<(), Box<dyn Error + Send + Sync>> = tokio::select! {
        result = &mut waiting => {
            result.map(|_| ()).map_err(Into::into)
        }
        signal = shutdown_signal() => {
            match signal {
                Ok(()) => {
                    state.begin_shutdown();
                    transport_cancellation.cancel();
                    match tokio::time::timeout(Duration::from_secs(5), &mut waiting).await {
                        Ok(result) => result.map(|_| ()).map_err(Into::into),
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "MCP transport did not stop within the shutdown deadline",
                        ).into()),
                    }
                }
                Err(error) => Err(error.into()),
            }
        }
    };

    let shutdown_result = tokio::time::timeout(Duration::from_secs(5), state.shutdown_async())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "domain cleanup did not stop within the shutdown deadline",
            )
        })?;
    transport_result?;
    shutdown_result?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
