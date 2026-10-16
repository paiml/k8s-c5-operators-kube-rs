//! Pure planning plus an executor that talks to the cluster through a trait.
use crate::crd::{Condition, Website, WebsiteSpec, WebsiteStatus};
use crate::Error;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{Container, ContainerPort, PodSpec, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::Resource;
use serde::Serialize;
use std::collections::BTreeMap;

/// Finalizer that makes deletion wait for cleanup.
pub const FINALIZER: &str = "sites.example.dev/cleanup";
/// Replicas used when the spec leaves them out.
pub const DEFAULT_REPLICAS: i32 = 1;

/// What the cluster currently has for a Deployment.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedDeployment {
    pub image: String,
    pub replicas: i32,
    pub ready_replicas: i32,
}

impl ObservedDeployment {
    /// Reads the fields the planner cares about from a real Deployment.
    pub fn from_deployment(d: &Deployment) -> Self {
        let spec = d.spec.as_ref();
        let image = spec
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|p| p.containers.first())
            .and_then(|c| c.image.clone())
            .unwrap_or_default();
        ObservedDeployment {
            image,
            replicas: spec.and_then(|s| s.replicas).unwrap_or(0),
            ready_replicas: d
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0),
        }
    }
}

/// Everything the planner needs to know about the world.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observed {
    pub deployment: Option<ObservedDeployment>,
    pub deleting: bool,
    pub has_finalizer: bool,
    pub status: WebsiteStatus,
}

/// One step of a plan.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    AddFinalizer,
    RemoveFinalizer,
    CreateDeployment { image: String, replicas: i32 },
    UpdateDeployment { image: String, replicas: i32 },
    DeleteDeployment,
    SetStatus { status: WebsiteStatus },
}

/// The status a Website should report given what is deployed.
pub fn desired_status(
    spec: &WebsiteSpec,
    deployment: Option<&ObservedDeployment>,
) -> WebsiteStatus {
    let want = spec.effective_replicas();
    let ready = deployment.map_or(0, |d| d.ready_replicas);
    let available = deployment.is_some() && ready >= want;
    let (status, reason) = if available {
        ("True", "AllReplicasReady")
    } else {
        ("False", "Progressing")
    };
    WebsiteStatus {
        ready_replicas: ready,
        conditions: vec![Condition {
            type_: "Available".into(),
            status: status.into(),
            reason: reason.into(),
            message: format!("{ready}/{want} replicas ready"),
        }],
    }
}

/// Decides what to do. Pure: the same inputs always give the same plan.
pub fn plan(spec: &WebsiteSpec, observed: &Observed) -> Vec<Action> {
    let mut actions = Vec::new();
    if observed.deleting {
        if observed.has_finalizer {
            if observed.deployment.is_some() {
                actions.push(Action::DeleteDeployment);
            }
            actions.push(Action::RemoveFinalizer);
        }
        return actions;
    }
    if !observed.has_finalizer {
        actions.push(Action::AddFinalizer);
    }
    let image = spec.image.clone();
    let replicas = spec.effective_replicas();
    match &observed.deployment {
        None => actions.push(Action::CreateDeployment { image, replicas }),
        Some(d) if d.image != image || d.replicas != replicas => {
            actions.push(Action::UpdateDeployment { image, replicas })
        }
        Some(_) => {}
    }
    let status = desired_status(spec, observed.deployment.as_ref());
    if status != observed.status {
        actions.push(Action::SetStatus { status });
    }
    actions
}

/// Builds the observed state of a Website from the object and its Deployment.
pub fn observe(site: &Website, deployment: Option<ObservedDeployment>) -> Observed {
    Observed {
        deployment,
        deleting: site.metadata.deletion_timestamp.is_some(),
        has_finalizer: site
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| f.iter().any(|x| x == FINALIZER)),
        status: site.status.clone().unwrap_or_default(),
    }
}

/// The Deployment a Website asks for.
pub fn desired_deployment(site: &Website) -> Deployment {
    let name = site.metadata.name.clone().unwrap_or_default();
    let labels: BTreeMap<String, String> = [
        ("app.kubernetes.io/name".to_string(), name.clone()),
        (
            "app.kubernetes.io/managed-by".to_string(),
            "opctl".to_string(),
        ),
    ]
    .into();
    let selector: BTreeMap<String, String> =
        [("app.kubernetes.io/name".to_string(), name.clone())].into();
    Deployment {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: site.metadata.namespace.clone(),
            labels: Some(labels.clone()),
            owner_references: site.controller_owner_ref(&()).map(|o| vec![o]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(site.spec.effective_replicas()),
            selector: LabelSelector {
                match_labels: Some(selector),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: "web".into(),
                        image: Some(site.spec.image.clone()),
                        ports: Some(vec![ContainerPort {
                            container_port: 80,
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The calls the reconciler makes on a cluster. `glue::KubeCluster` implements
/// it for a real API server; tests implement it in memory.
#[allow(async_fn_in_trait)]
pub trait Cluster {
    async fn get_deployment(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<ObservedDeployment>, Error>;
    async fn apply_deployment(&self, deployment: &Deployment) -> Result<(), Error>;
    async fn delete_deployment(&self, ns: &str, name: &str) -> Result<(), Error>;
    async fn set_finalizers(
        &self,
        ns: &str,
        name: &str,
        finalizers: Vec<String>,
    ) -> Result<(), Error>;
    async fn patch_status(&self, ns: &str, name: &str, status: &WebsiteStatus)
        -> Result<(), Error>;
}

/// Result of one reconcile pass.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub actions: Vec<Action>,
    /// Seconds until the next pass, or `None` to wait for a change.
    pub requeue_secs: Option<u64>,
}

/// Observes, plans and executes one pass for a Website.
pub async fn reconcile<C: Cluster>(cluster: &C, site: &Website) -> Result<Outcome, Error> {
    let name = site
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| Error::Invalid("Website has no name".into()))?;
    let ns = site
        .metadata
        .namespace
        .as_deref()
        .ok_or_else(|| Error::Invalid("Website has no namespace".into()))?;
    let deployment = cluster.get_deployment(ns, name).await?;
    let observed = observe(site, deployment);
    let actions = plan(&site.spec, &observed);
    let current: Vec<String> = site.metadata.finalizers.clone().unwrap_or_default();
    for action in &actions {
        match action {
            Action::AddFinalizer => {
                let mut f = current.clone();
                f.push(FINALIZER.to_string());
                cluster.set_finalizers(ns, name, f).await?;
            }
            Action::RemoveFinalizer => {
                let f = current
                    .iter()
                    .filter(|x| *x != FINALIZER)
                    .cloned()
                    .collect();
                cluster.set_finalizers(ns, name, f).await?;
            }
            Action::CreateDeployment { .. } | Action::UpdateDeployment { .. } => {
                cluster.apply_deployment(&desired_deployment(site)).await?;
            }
            Action::DeleteDeployment => cluster.delete_deployment(ns, name).await?,
            Action::SetStatus { status } => cluster.patch_status(ns, name, status).await?,
        }
    }
    let available =
        desired_status(&site.spec, observed.deployment.as_ref()).conditions[0].status == "True";
    let requeue_secs = if observed.deleting {
        None
    } else if available {
        Some(300)
    } else {
        Some(15)
    };
    Ok(Outcome {
        actions,
        requeue_secs,
    })
}
