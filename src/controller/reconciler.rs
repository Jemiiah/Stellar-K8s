//! Main reconciler for StellarNode resources
//!
//! Implements the controller pattern using kube-rs runtime.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Service};
use kube::{
    api::{Api, Patch, PatchParams},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        finalizer::{finalizer, Event},
        watcher::Config,
    },
    Resource, ResourceExt,
};
use tracing::{error, info, instrument, warn};

use crate::crd::{NodeType, StellarNode, StellarNodeStatus};
use crate::error::{Error, Result};

use super::finalizers::STELLAR_NODE_FINALIZER;
use super::resources;

/// Shared state for the controller
pub struct ControllerState {
    pub client: Client,
}

/// Main entry point to start the controller
pub async fn run_controller(state: Arc<ControllerState>) -> Result<()> {
    let client = state.client.clone();
    let stellar_nodes: Api<StellarNode> = Api::all(client.clone());

    info!("Starting StellarNode controller");

    // Verify CRD exists
    match stellar_nodes.list(&Default::default()).await {
        Ok(_) => info!("StellarNode CRD is available"),
        Err(e) => {
            error!(
                "StellarNode CRD not found. Please install the CRD first: {:?}",
                e
            );
            return Err(Error::ConfigError(
                "StellarNode CRD not installed".to_string(),
            ));
        }
    }

    Controller::new(stellar_nodes, Config::default())
        // Watch owned resources for changes
        .owns::<Deployment>(Api::all(client.clone()), Config::default())
        .owns::<StatefulSet>(Api::all(client.clone()), Config::default())
        .owns::<Service>(Api::all(client.clone()), Config::default())
        .owns::<PersistentVolumeClaim>(Api::all(client.clone()), Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, state)
        .for_each(|res| async move {
            match res {
                Ok(obj) => info!("Reconciled: {:?}", obj),
                Err(e) => error!("Reconcile error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

/// The main reconciliation function
///
/// This function is called whenever:
/// - A StellarNode is created, updated, or deleted
/// - An owned resource (Deployment, Service, PVC) changes
/// - The requeue timer expires
#[instrument(skip(ctx), fields(name = %obj.name_any(), namespace = obj.namespace()))]
async fn reconcile(obj: Arc<StellarNode>, ctx: Arc<ControllerState>) -> Result<Action> {
    let client = ctx.client.clone();
    let namespace = obj.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    info!(
        "Reconciling StellarNode {}/{} (type: {:?})",
        namespace,
        obj.name_any(),
        obj.spec.node_type
    );

    // Use kube-rs built-in finalizer helper for clean lifecycle management
    finalizer(&api, STELLAR_NODE_FINALIZER, obj, |event| async {
        match event {
            Event::Apply(node) => apply_stellar_node(&client, &node).await,
            Event::Cleanup(node) => cleanup_stellar_node(&client, &node).await,
        }
    })
    .await
    .map_err(Error::from)
}

/// Apply/create/update the StellarNode resources
async fn apply_stellar_node(client: &Client, node: &StellarNode) -> Result<Action> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    info!("Applying StellarNode: {}/{}", namespace, name);

    // Validate the spec
    if let Err(e) = node.spec.validate() {
        warn!("Validation failed for {}/{}: {}", namespace, name, e);
        update_status(client, node, "Failed", Some(&e)).await?;
        return Err(Error::ValidationError(e));
    }

    // 1. Core infrastructure (PVC and ConfigMap) always managed by operator
    resources::ensure_pvc(client, node).await?;
    resources::ensure_config_map(client, node).await?;

    // 2. Maintenance Mode Check
    // If active, we skip workload management (Step 3) and suspension checks.
    // This allows a human to manually scale the node up or down as needed.
    if node.spec.maintenance_mode {
        info!(
            "Node {}/{} in Maintenance Mode. Skipping workload updates.",
            namespace, name
        );

        resources::ensure_service(client, node).await?;

        update_status(
            client,
            node,
            "Maintenance",
            Some("Manual maintenance mode active; workload management paused"),
        )
        .await?;

        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    // 3. Normal Mode: Handle suspension
    // This only runs if NOT in maintenance mode.
    if node.spec.suspended {
        info!("Node {}/{} is suspended, scaling to 0", namespace, name);
        update_status(client, node, "Suspended", Some("Node is suspended")).await?;
    }

    // 4. Manage Workload (Deployment/StatefulSet)
    match node.spec.node_type {
        NodeType::Validator => {
            resources::ensure_statefulset(client, node).await?;
        }
        NodeType::Horizon | NodeType::SorobanRpc => {
            resources::ensure_deployment(client, node).await?;
        }
    }

    // 5. Ensure Service and finalize status
    resources::ensure_service(client, node).await?;

    let phase = if node.spec.suspended {
        "Suspended"
    } else {
        "Running"
    };
    update_status(client, node, phase, Some("Resources synchronized")).await?;

    Ok(Action::requeue(Duration::from_secs(30)))
}

/// Clean up resources when the StellarNode is deleted
async fn cleanup_stellar_node(client: &Client, node: &StellarNode) -> Result<Action> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    info!("Cleaning up StellarNode: {}/{}", namespace, name);

    // Delete resources in reverse order of creation

    // 1. Delete Service
    if let Err(e) = resources::delete_service(client, node).await {
        warn!("Failed to delete Service: {:?}", e);
    }

    // 2. Delete Deployment/StatefulSet
    if let Err(e) = resources::delete_workload(client, node).await {
        warn!("Failed to delete workload: {:?}", e);
    }

    // 3. Delete ConfigMap
    if let Err(e) = resources::delete_config_map(client, node).await {
        warn!("Failed to delete ConfigMap: {:?}", e);
    }

    // 4. Delete PVC based on retention policy
    if node.spec.should_delete_pvc() {
        info!(
            "Deleting PVC for node: {}/{} (retention policy: Delete)",
            namespace, name
        );
        if let Err(e) = resources::delete_pvc(client, node).await {
            warn!("Failed to delete PVC: {:?}", e);
        }
    } else {
        info!(
            "Retaining PVC for node: {}/{} (retention policy: Retain)",
            namespace, name
        );
    }

    info!("Cleanup complete for StellarNode: {}/{}", namespace, name);

    // Return await_change to signal finalizer completion
    Ok(Action::await_change())
}

/// Update the status subresource of a StellarNode
async fn update_status(
    client: &Client,
    node: &StellarNode,
    phase: &str,
    message: Option<&str>,
) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    let status = StellarNodeStatus {
        phase: phase.to_string(),
        message: message.map(String::from),
        observed_generation: node.metadata.generation,
        replicas: if node.spec.suspended {
            0
        } else {
            node.spec.replicas
        },
        ..Default::default()
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(
        &node.name_any(),
        &PatchParams::apply("stellar-operator"),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;

    Ok(())
}

/// Error policy determines how to handle reconciliation errors
fn error_policy(node: Arc<StellarNode>, error: &Error, _ctx: Arc<ControllerState>) -> Action {
    error!("Reconciliation error for {}: {:?}", node.name_any(), error);

    // Use shorter retry for retriable errors
    let retry_duration = if error.is_retriable() {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(60)
    };

    Action::requeue(retry_duration)
}
