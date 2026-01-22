//! Kubernetes resource builders for StellarNode
//!
//! This module creates and manages the underlying Kubernetes resources
//! (Deployments, StatefulSets, Services, PVCs, ConfigMaps) for each StellarNode.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSpec, PodTemplateSpec, ResourceRequirements as K8sResources, Service, ServicePort,
    ServiceSpec, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::{Client, Resource, ResourceExt};
use tracing::{info, warn};

use crate::crd::{NodeType, StellarNode};
use crate::error::{Error, Result};

/// Get the standard labels for a StellarNode's resources
fn standard_labels(node: &StellarNode) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        "stellar-node".to_string(),
    );
    labels.insert("app.kubernetes.io/instance".to_string(), node.name_any());
    labels.insert(
        "app.kubernetes.io/component".to_string(),
        node.spec.node_type.to_string().to_lowercase(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "stellar-operator".to_string(),
    );
    labels.insert(
        "stellar.org/node-type".to_string(),
        node.spec.node_type.to_string(),
    );
    labels
}

/// Create an OwnerReference for garbage collection
fn owner_reference(node: &StellarNode) -> OwnerReference {
    OwnerReference {
        api_version: StellarNode::api_version(&()).to_string(),
        kind: StellarNode::kind(&()).to_string(),
        name: node.name_any(),
        uid: node.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Build the resource name for a given component
fn resource_name(node: &StellarNode, suffix: &str) -> String {
    format!("{}-{}", node.name_any(), suffix)
}

// ============================================================================
// PersistentVolumeClaim
// ============================================================================

/// Ensure a PersistentVolumeClaim exists for the node
pub async fn ensure_pvc(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let name = resource_name(node, "data");

    let pvc = build_pvc(node);

    match api.get(&name).await {
        Ok(_existing) => {
            // PVCs are mostly immutable, just ensure it exists
            info!("PVC {} already exists", name);
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            info!("Creating PVC {}", name);
            api.create(&PostParams::default(), &pvc).await?;
        }
        Err(e) => return Err(Error::KubeError(e)),
    }

    Ok(())
}

fn build_pvc(node: &StellarNode) -> PersistentVolumeClaim {
    let labels = standard_labels(node);
    let name = resource_name(node, "data");

    let mut requests = BTreeMap::new();
    requests.insert(
        "storage".to_string(),
        Quantity(node.spec.storage.size.clone()),
    );

    // Merge custom annotations from storage config with existing annotations
    let annotations = node.spec.storage.annotations.clone().unwrap_or_default();

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: node.namespace(),
            labels: Some(labels),
            annotations: if annotations.is_empty() {
                None
            } else {
                Some(annotations)
            },
            owner_references: Some(vec![owner_reference(node)]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: Some(node.spec.storage.storage_class.clone()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: None,
    }
}

/// Delete the PersistentVolumeClaim for a node
pub async fn delete_pvc(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let name = resource_name(node, "data");

    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => info!("Deleted PVC {}", name),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!("PVC {} not found, already deleted", name);
        }
        Err(e) => return Err(Error::KubeError(e)),
    }

    Ok(())
}

// ============================================================================
// ConfigMap
// ============================================================================

/// Ensure a ConfigMap exists with node configuration
pub async fn ensure_config_map(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
    let name = resource_name(node, "config");

    let cm = build_config_map(node);

    let patch = Patch::Apply(&cm);
    api.patch(&name, &PatchParams::apply("stellar-operator"), &patch)
        .await?;

    Ok(())
}

fn build_config_map(node: &StellarNode) -> ConfigMap {
    let labels = standard_labels(node);
    let name = resource_name(node, "config");

    let mut data = BTreeMap::new();

    // Add network-specific configuration
    data.insert(
        "NETWORK_PASSPHRASE".to_string(),
        node.spec.network.passphrase().to_string(),
    );

    // Add node-type-specific configuration
    match &node.spec.node_type {
        NodeType::Validator => {
            if let Some(config) = &node.spec.validator_config {
                if let Some(quorum) = &config.quorum_set {
                    data.insert("stellar-core.cfg".to_string(), quorum.clone());
                }
            }
        }
        NodeType::Horizon => {
            if let Some(config) = &node.spec.horizon_config {
                data.insert(
                    "STELLAR_CORE_URL".to_string(),
                    config.stellar_core_url.clone(),
                );
                data.insert("INGEST".to_string(), config.enable_ingest.to_string());
            }
        }
        NodeType::SorobanRpc => {
            if let Some(config) = &node.spec.soroban_config {
                data.insert(
                    "STELLAR_CORE_URL".to_string(),
                    config.stellar_core_url.clone(),
                );
                if let Some(captive_config) = &config.captive_core_config {
                    data.insert("captive-core.cfg".to_string(), captive_config.clone());
                }
            }
        }
    }

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: node.namespace(),
            labels: Some(labels),
            owner_references: Some(vec![owner_reference(node)]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    }
}

/// Delete the ConfigMap for a node
pub async fn delete_config_map(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
    let name = resource_name(node, "config");

    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => info!("Deleted ConfigMap {}", name),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!("ConfigMap {} not found", name);
        }
        Err(e) => return Err(Error::KubeError(e)),
    }

    Ok(())
}

// ============================================================================
// Deployment (for Horizon and Soroban RPC)
// ============================================================================

/// Ensure a Deployment exists for RPC nodes
pub async fn ensure_deployment(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    let name = node.name_any();

    let deployment = build_deployment(node);

    let patch = Patch::Apply(&deployment);
    api.patch(
        &name,
        &PatchParams::apply("stellar-operator").force(),
        &patch,
    )
    .await?;

    Ok(())
}

fn build_deployment(node: &StellarNode) -> Deployment {
    let labels = standard_labels(node);
    let name = node.name_any();

    let replicas = if node.spec.suspended {
        0
    } else {
        node.spec.replicas
    };

    Deployment {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: node.namespace(),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_reference(node)]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: build_pod_template(node, &labels),
            ..Default::default()
        }),
        status: None,
    }
}

// ============================================================================
// StatefulSet (for Validators)
// ============================================================================

/// Ensure a StatefulSet exists for Validator nodes
pub async fn ensure_statefulset(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StatefulSet> = Api::namespaced(client.clone(), &namespace);
    let name = node.name_any();

    let statefulset = build_statefulset(node);

    let patch = Patch::Apply(&statefulset);
    api.patch(
        &name,
        &PatchParams::apply("stellar-operator").force(),
        &patch,
    )
    .await?;

    Ok(())
}

fn build_statefulset(node: &StellarNode) -> StatefulSet {
    let labels = standard_labels(node);
    let name = node.name_any();

    let replicas = if node.spec.suspended { 0 } else { 1 }; // Validators always have 1 replica

    StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: node.namespace(),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_reference(node)]),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            service_name: format!("{}-headless", name),
            template: build_pod_template(node, &labels),
            ..Default::default()
        }),
        status: None,
    }
}

/// Delete the workload (Deployment or StatefulSet) for a node
pub async fn delete_workload(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    match node.spec.node_type {
        NodeType::Validator => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), &namespace);
            match api.delete(&name, &DeleteParams::default()).await {
                Ok(_) => info!("Deleted StatefulSet {}", name),
                Err(kube::Error::Api(e)) if e.code == 404 => {
                    warn!("StatefulSet {} not found", name);
                }
                Err(e) => return Err(Error::KubeError(e)),
            }
        }
        _ => {
            let api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
            match api.delete(&name, &DeleteParams::default()).await {
                Ok(_) => info!("Deleted Deployment {}", name),
                Err(kube::Error::Api(e)) if e.code == 404 => {
                    warn!("Deployment {} not found", name);
                }
                Err(e) => return Err(Error::KubeError(e)),
            }
        }
    }

    Ok(())
}

// ============================================================================
// Service
// ============================================================================

/// Ensure a Service exists for the node
pub async fn ensure_service(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let name = node.name_any();

    let service = build_service(node);

    let patch = Patch::Apply(&service);
    api.patch(
        &name,
        &PatchParams::apply("stellar-operator").force(),
        &patch,
    )
    .await?;

    Ok(())
}

fn build_service(node: &StellarNode) -> Service {
    let labels = standard_labels(node);
    let name = node.name_any();

    let ports = match node.spec.node_type {
        NodeType::Validator => vec![
            ServicePort {
                name: Some("peer".to_string()),
                port: 11625,
                ..Default::default()
            },
            ServicePort {
                name: Some("http".to_string()),
                port: 11626,
                ..Default::default()
            },
        ],
        NodeType::Horizon => vec![ServicePort {
            name: Some("http".to_string()),
            port: 8000,
            ..Default::default()
        }],
        NodeType::SorobanRpc => vec![ServicePort {
            name: Some("http".to_string()),
            port: 8000,
            ..Default::default()
        }],
    };

    Service {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: node.namespace(),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_reference(node)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(labels),
            ports: Some(ports),
            ..Default::default()
        }),
        status: None,
    }
}

/// Delete the Service for a node
pub async fn delete_service(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let name = node.name_any();

    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => info!("Deleted Service {}", name),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!("Service {} not found", name);
        }
        Err(e) => return Err(Error::KubeError(e)),
    }

    Ok(())
}

// ============================================================================
// Pod Template Builder
// ============================================================================

fn build_pod_template(node: &StellarNode, labels: &BTreeMap<String, String>) -> PodTemplateSpec {
    let container = build_container(node);

    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            containers: vec![container],
            volumes: Some(vec![
                Volume {
                    name: "data".to_string(),
                    persistent_volume_claim: Some(
                        k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                            claim_name: resource_name(node, "data"),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                },
                Volume {
                    name: "config".to_string(),
                    config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                        name: Some(resource_name(node, "config")),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
    }
}

fn build_container(node: &StellarNode) -> Container {
    let mut requests = BTreeMap::new();
    requests.insert(
        "cpu".to_string(),
        Quantity(node.spec.resources.requests.cpu.clone()),
    );
    requests.insert(
        "memory".to_string(),
        Quantity(node.spec.resources.requests.memory.clone()),
    );

    let mut limits = BTreeMap::new();
    limits.insert(
        "cpu".to_string(),
        Quantity(node.spec.resources.limits.cpu.clone()),
    );
    limits.insert(
        "memory".to_string(),
        Quantity(node.spec.resources.limits.memory.clone()),
    );

    let (container_port, data_mount_path) = match node.spec.node_type {
        NodeType::Validator => (11625, "/opt/stellar/data"),
        NodeType::Horizon => (8000, "/data"),
        NodeType::SorobanRpc => (8000, "/data"),
    };

    Container {
        name: "stellar-node".to_string(),
        image: Some(node.spec.container_image()),
        ports: Some(vec![ContainerPort {
            container_port,
            ..Default::default()
        }]),
        env: Some(vec![EnvVar {
            name: "NETWORK_PASSPHRASE".to_string(),
            value: Some(node.spec.network.passphrase().to_string()),
            ..Default::default()
        }]),
        resources: Some(K8sResources {
            requests: Some(requests),
            limits: Some(limits),
            claims: None,
        }),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "data".to_string(),
                mount_path: data_mount_path.to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: "config".to_string(),
                mount_path: "/config".to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        ..Default::default()
    }
}
