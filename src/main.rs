//! Stellar-K8s Operator Entry Point
//!
//! Starts the Kubernetes controller and optional REST API server.

use std::sync::Arc;

use stellar_k8s::{controller, Error};
use tracing::{info, Level};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(true))
        .with(
            EnvFilter::builder()
                .with_default_directive(Level::INFO.into())
                .from_env_lossy(),
        )
        .init();

    info!(
        "Starting Stellar-K8s Operator v{}",
        env!("CARGO_PKG_VERSION")
    );

    // Initialize Kubernetes client
    let client = kube::Client::try_default()
        .await
        .map_err(|e| Error::KubeError(e))?;

    info!("Connected to Kubernetes cluster");

    // Create shared controller state
    let state = Arc::new(controller::ControllerState {
        client: client.clone(),
    });

    // Start the controller
    // In production, you might also start the REST API server here
    #[cfg(feature = "rest-api")]
    {
        let api_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = stellar_k8s::rest_api::run_server(api_state).await {
                tracing::error!("REST API server error: {:?}", e);
            }
        });
    }

    // Run the main controller loop
    controller::run_controller(state).await?;

    Ok(())
}
