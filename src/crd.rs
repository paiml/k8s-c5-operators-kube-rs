//! The `Website` custom resource.
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Desired state of a Website.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "sites.example.dev",
    version = "v1",
    kind = "Website",
    namespaced,
    status = "WebsiteStatus",
    shortname = "site"
)]
pub struct WebsiteSpec {
    /// Container image to serve, for example `nginx:1.27`.
    pub image: String,
    /// Number of replicas. Defaults to 1 through the mutating webhook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
}

/// One status condition.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Condition {
    /// Condition type, for example `Available`.
    #[serde(rename = "type")]
    pub type_: String,
    /// `True` or `False`.
    pub status: String,
    /// Short machine-readable reason.
    pub reason: String,
    /// Human-readable message.
    pub message: String,
}

/// Observed state of a Website.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebsiteStatus {
    /// Replicas of the Deployment that are ready.
    pub ready_replicas: i32,
    /// Conditions, currently only `Available`.
    pub conditions: Vec<Condition>,
}

impl WebsiteSpec {
    /// Replicas to run: the spec value, or 1 when unset.
    pub fn effective_replicas(&self) -> i32 {
        self.replicas.unwrap_or(crate::reconcile::DEFAULT_REPLICAS)
    }
}

/// The generated CustomResourceDefinition as YAML.
pub fn crd_yaml() -> String {
    use kube::CustomResourceExt;
    serde_yaml::to_string(&Website::crd()).expect("a CRD serialises to YAML")
}
