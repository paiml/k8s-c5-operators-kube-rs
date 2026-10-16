//! The only module that talks to a real API server. Kept thin on purpose: it
//! maps the `Cluster` trait onto kube-rs calls and starts the controller loop.
//! It is excluded from the coverage run; see the README.
use crate::cli::parse_run_args;
use crate::crd::{Website, WebsiteStatus};
use crate::reconcile::{reconcile, Cluster, ObservedDeployment};
use crate::Error;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::{watcher, Controller};
use kube::Client;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn cluster_err(e: kube::Error) -> Error {
    Error::Cluster(e.to_string())
}

/// An object that is already gone needs no further work.
fn gone_ok<T>(r: Result<T, kube::Error>) -> Result<(), Error> {
    match r {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
        Err(e) => Err(cluster_err(e)),
    }
}

/// `Cluster` backed by a real client.
pub struct KubeCluster(pub Client);

impl Cluster for KubeCluster {
    async fn get_deployment(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<ObservedDeployment>, Error> {
        let api: Api<Deployment> = Api::namespaced(self.0.clone(), ns);
        let d = api.get_opt(name).await.map_err(cluster_err)?;
        Ok(d.as_ref().map(ObservedDeployment::from_deployment))
    }
    async fn apply_deployment(&self, d: &Deployment) -> Result<(), Error> {
        let ns = d.metadata.namespace.as_deref().unwrap_or("default");
        let name = d.metadata.name.as_deref().unwrap_or_default();
        let api: Api<Deployment> = Api::namespaced(self.0.clone(), ns);
        let pp = PatchParams::apply("opctl").force();
        api.patch(name, &pp, &Patch::Apply(d))
            .await
            .map_err(cluster_err)?;
        Ok(())
    }
    async fn delete_deployment(&self, ns: &str, name: &str) -> Result<(), Error> {
        let api: Api<Deployment> = Api::namespaced(self.0.clone(), ns);
        gone_ok(api.delete(name, &DeleteParams::default()).await)
    }
    async fn set_finalizers(
        &self,
        ns: &str,
        name: &str,
        finalizers: Vec<String>,
    ) -> Result<(), Error> {
        let api: Api<Website> = Api::namespaced(self.0.clone(), ns);
        let body = json!({ "metadata": { "finalizers": finalizers } });
        gone_ok(
            api.patch(name, &PatchParams::default(), &Patch::Merge(&body))
                .await,
        )
    }
    async fn patch_status(
        &self,
        ns: &str,
        name: &str,
        status: &WebsiteStatus,
    ) -> Result<(), Error> {
        let api: Api<Website> = Api::namespaced(self.0.clone(), ns);
        let body = json!({ "status": status });
        gone_ok(
            api.patch_status(name, &PatchParams::default(), &Patch::Merge(&body))
                .await,
        )
    }
}

async fn reconcile_one(site: Arc<Website>, client: Arc<Client>) -> Result<Action, Error> {
    let cluster = KubeCluster((*client).clone());
    let outcome = reconcile(&cluster, &site).await?;
    println!(
        "reconciled {} -> {} action(s)",
        site.metadata.name.as_deref().unwrap_or("?"),
        outcome.actions.len()
    );
    Ok(outcome.requeue_secs.map_or_else(Action::await_change, |s| {
        Action::requeue(Duration::from_secs(s))
    }))
}

fn on_error(_site: Arc<Website>, e: &Error, _client: Arc<Client>) -> Action {
    eprintln!("opctl: reconcile failed: {e}");
    Action::requeue(Duration::from_secs(10))
}

/// Runs the controller until the process is stopped. Returns an exit code.
pub async fn run(args: &[String]) -> i32 {
    let namespace = match parse_run_args(args) {
        Ok(ns) => ns,
        Err(e) => {
            eprintln!("opctl: {e}");
            return 2;
        }
    };
    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("opctl: cannot reach a cluster: {e}");
            return 1;
        }
    };
    let (sites, deployments): (Api<Website>, Api<Deployment>) = match &namespace {
        Some(ns) => (
            Api::namespaced(client.clone(), ns),
            Api::namespaced(client.clone(), ns),
        ),
        None => (Api::all(client.clone()), Api::all(client.clone())),
    };
    println!(
        "opctl: controller started ({})",
        namespace.as_deref().unwrap_or("all namespaces")
    );
    Controller::new(sites, watcher::Config::default())
        .owns(deployments, watcher::Config::default())
        .run(reconcile_one, on_error, Arc::new(client))
        .for_each(|_| async {})
        .await;
    0
}
