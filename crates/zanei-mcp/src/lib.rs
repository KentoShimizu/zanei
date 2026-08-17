//! Read-only MCP server exposed by `zanei mcp`.

mod server;

use std::path::PathBuf;

use rmcp::ServiceExt;
use rmcp::service::ServerInitializeError;
use rmcp::transport::stdio;
use thiserror::Error;
use zanei_core::config::Config;

use server::ZaneiServer;

pub type PermissionCheck = fn(&Config) -> Result<bool, String>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to create the MCP async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize the MCP stdio service: {0}")]
    Initialize(#[source] Box<ServerInitializeError>),
    #[error("MCP stdio service task failed: {0}")]
    Service(#[source] tokio::task::JoinError),
}

/// Runs the read-only MCP server until its stdio transport closes.
pub fn run(
    store_path: impl Into<PathBuf>,
    config_path: impl Into<PathBuf>,
    permission_check: PermissionCheck,
) -> Result<(), ServerError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(ServerError::Runtime)?;
    let server = ZaneiServer::new(store_path.into(), config_path.into(), permission_check);

    runtime.block_on(async move {
        let service = server
            .serve(stdio())
            .await
            .map_err(|error| ServerError::Initialize(Box::new(error)))?;
        service.waiting().await.map_err(ServerError::Service)?;
        Ok(())
    })
}
