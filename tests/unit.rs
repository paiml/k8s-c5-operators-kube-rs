use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use opctl::admission::{self, image_problem, mutate, mutation_ops, validate, violations};
use opctl::cli;
use opctl::crd::{crd_yaml, Website, WebsiteSpec, WebsiteStatus};
use opctl::reconcile::*;
use opctl::Error;
use serde_json::{json, Value};
use std::sync::Mutex;

// ---------- helpers ----------

fn run_cli(a: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = a.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let rc = cli::run(&args, &mut out, &mut err);
    (
        rc,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn spec(image: &str, replicas: Option<i32>) -> WebsiteSpec {
    WebsiteSpec {
        image: image.into(),
        replicas,
    }
}

fn site(image: &str, replicas: Option<i32>) -> Website {
    let mut s = Website::new("hello", spec(image, replicas));
    s.metadata.namespace = Some("demo".into());
    s.metadata.uid = Some("uid-1".into());
    s
}

fn review(op: &str, object: Option<Value>) -> String {
    let mut request = json!({
        "uid": "abc-123",
        "kind": {"group": "sites.example.dev", "version": "v1", "kind": "Website"},
        "resource": {"group": "sites.example.dev", "version": "v1", "resource": "websites"},
        "operation": op,
        "userInfo": {"username": "tester"}
    });
    if let Some(o) = object {
        request["object"] = o;
    }
    json!({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview", "request": request})
        .to_string()
}

fn website_obj(spec: Value, labels: Option<Value>) -> Value {
    let mut meta = json!({"name": "hello", "namespace": "demo"});
    if let Some(l) = labels {
        meta["labels"] = l;
    }
    json!({"apiVersion": "sites.example.dev/v1", "kind": "Website", "metadata": meta, "spec": spec})
}

fn write_tmp(name: &str, text: &str) -> String {
    let p = std::env::temp_dir().join(format!("opctl-test-{}-{name}", std::process::id()));
    std::fs::write(&p, text).unwrap();
    p.to_str().unwrap().to_string()
}

fn od(image: &str, replicas: i32, ready: i32) -> ObservedDeployment {
    ObservedDeployment {
        image: image.into(),
        replicas,
        ready_replicas: ready,
    }
}

// ---------- in-memory cluster ----------

#[derive(Default)]
struct Mock {
    deployment: Mutex<Option<ObservedDeployment>>,
    finalizers: Mutex<Option<Vec<String>>>,
    status: Mutex<Option<WebsiteStatus>>,
    calls: Mutex<Vec<String>>,
    fail_on: Option<&'static str>,
}

impl Mock {
    fn step(&self, name: &str) -> Result<(), Error> {
        self.calls.lock().unwrap().push(name.to_string());
        if self.fail_on == Some(name) {
            return Err(Error::Cluster(format!("{name} refused")));
        }
        Ok(())
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Cluster for Mock {
    async fn get_deployment(
        &self,
        _ns: &str,
        _name: &str,
    ) -> Result<Option<ObservedDeployment>, Error> {
        self.step("get")?;
        Ok(self.deployment.lock().unwrap().clone())
    }
    async fn apply_deployment(&self, d: &Deployment) -> Result<(), Error> {
        self.step("apply")?;
        *self.deployment.lock().unwrap() = Some(ObservedDeployment::from_deployment(d));
        Ok(())
    }
    async fn delete_deployment(&self, _ns: &str, _name: &str) -> Result<(), Error> {
        self.step("delete")?;
        *self.deployment.lock().unwrap() = None;
        Ok(())
    }
    async fn set_finalizers(&self, _ns: &str, _name: &str, f: Vec<String>) -> Result<(), Error> {
        self.step("finalizers")?;
        *self.finalizers.lock().unwrap() = Some(f);
        Ok(())
    }
    async fn patch_status(&self, _ns: &str, _name: &str, s: &WebsiteStatus) -> Result<(), Error> {
        self.step("status")?;
        *self.status.lock().unwrap() = Some(s.clone());
        Ok(())
    }
}

/// What the API server would now hold for the Website after the mock saw patches.
fn refresh(site: &mut Website, m: &Mock) {
    if let Some(f) = m.finalizers.lock().unwrap().clone() {
        site.metadata.finalizers = Some(f);
    }
    if let Some(s) = m.status.lock().unwrap().clone() {
        site.status = Some(s);
    }
}

fn deleting(mut s: Website) -> Website {
    s.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH));
    s
}

// ---------- CRD ----------

#[test]
fn crd_yaml_describes_website() {
    let y: Value = serde_yaml::from_str(&crd_yaml()).unwrap();
    assert_eq!(y["kind"], "CustomResourceDefinition");
    assert_eq!(y["metadata"]["name"], "websites.sites.example.dev");
    assert_eq!(y["spec"]["scope"], "Namespaced");
    let v = &y["spec"]["versions"][0];
    assert_eq!(v["name"], "v1");
    assert_eq!(v["subresources"]["status"], json!({}));
    let spec = &v["schema"]["openAPIV3Schema"]["properties"]["spec"];
    assert_eq!(spec["required"], json!(["image"]));
    assert_eq!(spec["properties"]["replicas"]["type"], "integer");
    let status = &v["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
    assert_eq!(status["readyReplicas"]["type"], "integer");
    assert_eq!(status["conditions"]["type"], "array");
}

#[test]
fn effective_replicas_defaults_to_one() {
    assert_eq!(spec("a:1", None).effective_replicas(), 1);
    assert_eq!(spec("a:1", Some(4)).effective_replicas(), 4);
    assert_eq!(spec("a:1", Some(0)).effective_replicas(), 0);
}

#[test]
fn error_messages_name_their_kind() {
    assert_eq!(Error::Invalid("x".into()).to_string(), "invalid object: x");
    assert_eq!(Error::Cluster("x".into()).to_string(), "cluster error: x");
    assert_eq!(Error::Parse("x".into()).to_string(), "parse error: x");
}

// ---------- plan ----------

#[test]
fn plan_fresh_site_finalizes_creates_and_reports_progress() {
    let acts = plan(&spec("nginx:1.27", Some(2)), &Observed::default());
    assert_eq!(acts.len(), 3);
    assert_eq!(acts[0], Action::AddFinalizer);
    assert_eq!(
        acts[1],
        Action::CreateDeployment {
            image: "nginx:1.27".into(),
            replicas: 2
        }
    );
    match &acts[2] {
        Action::SetStatus { status } => {
            assert_eq!(status.ready_replicas, 0);
            assert_eq!(status.conditions[0].status, "False");
            assert_eq!(status.conditions[0].message, "0/2 replicas ready");
        }
        other => panic!("expected SetStatus, got {other:?}"),
    }
}

#[test]
fn plan_updates_on_image_or_replica_drift() {
    let base = Observed {
        deployment: Some(od("nginx:1.26", 2, 2)),
        has_finalizer: true,
        status: desired_status(&spec("nginx:1.27", Some(2)), Some(&od("nginx:1.26", 2, 2))),
        ..Default::default()
    };
    let by_image = plan(&spec("nginx:1.27", Some(2)), &base);
    assert_eq!(
        by_image,
        vec![Action::UpdateDeployment {
            image: "nginx:1.27".into(),
            replicas: 2
        }]
    );
    let by_replicas = plan(&spec("nginx:1.26", Some(3)), &base);
    assert!(matches!(
        by_replicas[0],
        Action::UpdateDeployment { replicas: 3, .. }
    ));
}

#[test]
fn plan_is_empty_when_converged() {
    let s = spec("nginx:1.27", None);
    let d = od("nginx:1.27", 1, 1);
    let observed = Observed {
        status: desired_status(&s, Some(&d)),
        deployment: Some(d),
        has_finalizer: true,
        deleting: false,
    };
    assert_eq!(plan(&s, &observed), vec![]);
}

#[test]
fn desired_status_available_only_when_all_ready() {
    let s = spec("a:1", Some(3));
    let partial = desired_status(&s, Some(&od("a:1", 3, 2)));
    assert_eq!(
        (
            partial.ready_replicas,
            partial.conditions[0].status.as_str()
        ),
        (2, "False")
    );
    assert_eq!(partial.conditions[0].reason, "Progressing");
    let full = desired_status(&s, Some(&od("a:1", 3, 3)));
    assert_eq!(
        (
            full.conditions[0].status.as_str(),
            full.conditions[0].reason.as_str()
        ),
        ("True", "AllReplicasReady")
    );
    // zero wanted and no deployment is still not available: nothing exists yet.
    assert_eq!(
        desired_status(&spec("a:1", Some(0)), None).conditions[0].status,
        "False"
    );
}

#[test]
fn plan_deletion_cleans_up_then_releases_finalizer() {
    let s = spec("a:1", None);
    let with_dep = Observed {
        deleting: true,
        has_finalizer: true,
        deployment: Some(od("a:1", 1, 1)),
        ..Default::default()
    };
    assert_eq!(
        plan(&s, &with_dep),
        vec![Action::DeleteDeployment, Action::RemoveFinalizer]
    );
    let no_dep = Observed {
        deleting: true,
        has_finalizer: true,
        ..Default::default()
    };
    assert_eq!(plan(&s, &no_dep), vec![Action::RemoveFinalizer]);
    let released = Observed {
        deleting: true,
        has_finalizer: false,
        deployment: Some(od("a:1", 1, 1)),
        ..Default::default()
    };
    assert_eq!(plan(&s, &released), vec![]);
}

#[test]
fn observe_reads_finalizer_deletion_and_status() {
    let mut s = site("a:1", None);
    assert_eq!(observe(&s, None), Observed::default());
    s.metadata.finalizers = Some(vec!["other".into(), FINALIZER.into()]);
    s.status = Some(WebsiteStatus {
        ready_replicas: 4,
        conditions: vec![],
    });
    let o = observe(&deleting(s), Some(od("a:1", 1, 1)));
    assert!(o.has_finalizer && o.deleting);
    assert_eq!(o.status.ready_replicas, 4);
    assert_eq!(o.deployment, Some(od("a:1", 1, 1)));
    let mut other = site("a:1", None);
    other.metadata.finalizers = Some(vec!["other".into()]);
    assert!(!observe(&other, None).has_finalizer);
}

// ---------- Deployment building ----------

#[test]
fn desired_deployment_carries_image_replicas_labels_and_owner() {
    let d = desired_deployment(&site("nginx:1.27", Some(4)));
    assert_eq!(d.metadata.name.as_deref(), Some("hello"));
    assert_eq!(d.metadata.namespace.as_deref(), Some("demo"));
    let owner = &d.metadata.owner_references.as_ref().unwrap()[0];
    assert_eq!(
        (owner.kind.as_str(), owner.name.as_str(), owner.uid.as_str()),
        ("Website", "hello", "uid-1")
    );
    assert_eq!(owner.controller, Some(true));
    let spec = d.spec.as_ref().unwrap();
    assert_eq!(spec.replicas, Some(4));
    assert_eq!(
        spec.selector.match_labels.as_ref().unwrap()["app.kubernetes.io/name"],
        "hello"
    );
    let pod = spec.template.spec.as_ref().unwrap();
    assert_eq!(pod.containers[0].image.as_deref(), Some("nginx:1.27"));
    assert_eq!(
        d.metadata.labels.as_ref().unwrap()["app.kubernetes.io/managed-by"],
        "opctl"
    );
    // round trip through the observer
    assert_eq!(
        ObservedDeployment::from_deployment(&d),
        od("nginx:1.27", 4, 0)
    );
}

#[test]
fn desired_deployment_without_uid_has_no_owner() {
    let mut s = site("a:1", None);
    s.metadata.uid = None;
    assert!(desired_deployment(&s).metadata.owner_references.is_none());
}

#[test]
fn observed_deployment_from_empty_deployment_is_zeroed() {
    assert_eq!(
        ObservedDeployment::from_deployment(&Deployment::default()),
        od("", 0, 0)
    );
}

// ---------- reconcile against the mock ----------

#[tokio::test]
async fn reconcile_converges_over_passes() {
    let m = Mock::default();
    let mut s = site("nginx:1.27", Some(2));

    let first = reconcile(&m, &s).await.unwrap();
    assert_eq!(first.actions.len(), 3);
    assert_eq!(first.requeue_secs, Some(15));
    assert_eq!(m.calls(), ["get", "finalizers", "apply", "status"]);
    assert_eq!(m.finalizers.lock().unwrap().clone().unwrap(), [FINALIZER]);
    assert_eq!(
        m.deployment.lock().unwrap().clone().unwrap(),
        od("nginx:1.27", 2, 0)
    );
    refresh(&mut s, &m);

    // the Deployment becomes ready
    m.deployment
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .ready_replicas = 2;
    let second = reconcile(&m, &s).await.unwrap();
    assert!(
        matches!(&second.actions[..], [Action::SetStatus { status }] if status.ready_replicas == 2)
    );
    refresh(&mut s, &m);
    assert_eq!(s.status.as_ref().unwrap().conditions[0].status, "True");

    // steady state: nothing to do, long requeue
    let third = reconcile(&m, &s).await.unwrap();
    assert_eq!(
        third,
        Outcome {
            actions: vec![],
            requeue_secs: Some(300)
        }
    );

    // someone edits the spec: the Deployment follows
    s.spec.replicas = Some(3);
    let fourth = reconcile(&m, &s).await.unwrap();
    assert!(matches!(
        fourth.actions[0],
        Action::UpdateDeployment { replicas: 3, .. }
    ));
    assert_eq!(m.deployment.lock().unwrap().clone().unwrap().replicas, 3);
}

#[tokio::test]
async fn reconcile_deletion_removes_deployment_and_finalizer() {
    let m = Mock::default();
    *m.deployment.lock().unwrap() = Some(od("a:1", 1, 1));
    let mut s = site("a:1", None);
    s.metadata.finalizers = Some(vec![FINALIZER.into(), "keep-me".into()]);
    let out = reconcile(&m, &deleting(s)).await.unwrap();
    assert_eq!(
        out,
        Outcome {
            actions: vec![Action::DeleteDeployment, Action::RemoveFinalizer],
            requeue_secs: None
        }
    );
    assert!(m.deployment.lock().unwrap().is_none());
    assert_eq!(m.finalizers.lock().unwrap().clone().unwrap(), ["keep-me"]);
}

#[tokio::test]
async fn reconcile_rejects_unnamed_objects() {
    let m = Mock::default();
    let mut s = site("a:1", None);
    s.metadata.namespace = None;
    assert!(
        matches!(reconcile(&m, &s).await, Err(Error::Invalid(msg)) if msg.contains("namespace"))
    );
    s.metadata.namespace = Some("demo".into());
    s.metadata.name = None;
    assert!(matches!(reconcile(&m, &s).await, Err(Error::Invalid(msg)) if msg.contains("name")));
    assert!(
        m.calls().is_empty(),
        "nothing is called for an invalid object"
    );
}

#[tokio::test]
async fn reconcile_stops_at_the_first_cluster_error() {
    for failing in ["get", "finalizers", "apply", "status"] {
        let m = Mock {
            fail_on: Some(failing),
            ..Default::default()
        };
        let err = reconcile(&m, &site("a:1", None)).await.unwrap_err();
        assert_eq!(err, Error::Cluster(format!("{failing} refused")));
        assert_eq!(m.calls().last().map(String::as_str), Some(failing));
    }
    // delete failure on the deletion path
    let m = Mock {
        fail_on: Some("delete"),
        ..Default::default()
    };
    *m.deployment.lock().unwrap() = Some(od("a:1", 1, 1));
    let mut s = site("a:1", None);
    s.metadata.finalizers = Some(vec![FINALIZER.into()]);
    assert!(reconcile(&m, &deleting(s)).await.is_err());
    assert!(
        m.finalizers.lock().unwrap().is_none(),
        "finalizer stays when cleanup fails"
    );
}

// ---------- admission ----------

#[test]
fn image_rules() {
    assert_eq!(image_problem("nginx:1.27"), None);
    assert_eq!(image_problem("localhost:5000/team/app:v2"), None);
    assert_eq!(image_problem("ghcr.io/o/app@sha256:0123abcdef"), None);
    assert!(image_problem("nginx:latest").unwrap().contains(":latest"));
    assert!(image_problem("nginx").unwrap().contains("no tag"));
    assert!(
        image_problem("localhost:5000/app")
            .unwrap()
            .contains("no tag"),
        "a registry port is not a tag"
    );
    assert!(image_problem("nginx:").unwrap().contains("empty tag"));
    assert!(image_problem("").unwrap().contains("must not be empty"));
    assert!(image_problem("app@sha256:")
        .unwrap()
        .contains("malformed digest"));
    assert!(image_problem("app@nothex")
        .unwrap()
        .contains("malformed digest"));
    assert!(image_problem("app@sha256:zz")
        .unwrap()
        .contains("malformed digest"));
    assert!(image_problem("app@:abcd")
        .unwrap()
        .contains("malformed digest"));
}

#[test]
fn violations_cover_image_and_replicas() {
    let ok = website_obj(json!({"image": "nginx:1.27", "replicas": 5}), None);
    assert!(violations(&ok).is_empty(), "5 is the limit and is allowed");
    assert!(violations(&website_obj(json!({"image": "nginx:1.27"}), None)).is_empty());
    let six = violations(&website_obj(
        json!({"image": "nginx:1.27", "replicas": 6}),
        None,
    ));
    assert_eq!(six, ["spec.replicas 6 exceeds the maximum of 5"]);
    let neg = violations(&website_obj(
        json!({"image": "nginx:1.27", "replicas": -1}),
        None,
    ));
    assert_eq!(neg, ["spec.replicas -1 must not be negative"]);
    let text = violations(&website_obj(
        json!({"image": "nginx:1.27", "replicas": "many"}),
        None,
    ));
    assert_eq!(text, ["spec.replicas must be an integer"]);
    let noimg = violations(&website_obj(json!({"replicas": 1}), None));
    assert_eq!(noimg, ["spec.image is required"]);
    let both = violations(&website_obj(json!({"image": "nginx", "replicas": 9}), None));
    assert_eq!(both.len(), 2);
}

fn response(text: &str) -> Value {
    serde_json::from_str::<Value>(text).unwrap()["response"].clone()
}

#[test]
fn validate_allows_a_good_website() {
    let r = review(
        "CREATE",
        Some(website_obj(
            json!({"image": "nginx:1.27", "replicas": 3}),
            None,
        )),
    );
    let (text, allowed) = validate(&r).unwrap();
    assert!(allowed);
    let resp = response(&text);
    assert_eq!(resp["allowed"], true);
    assert_eq!(
        resp["uid"], "abc-123",
        "the uid is echoed so the API server can match it"
    );
    assert!(resp.get("patch").is_none());
}

#[test]
fn validate_denies_with_a_message_and_403() {
    let r = review(
        "CREATE",
        Some(website_obj(
            json!({"image": "nginx:latest", "replicas": 9}),
            None,
        )),
    );
    let (text, allowed) = validate(&r).unwrap();
    assert!(!allowed);
    let resp = response(&text);
    assert_eq!(resp["allowed"], false);
    assert_eq!(resp["status"]["code"], 403);
    let msg = resp["status"]["message"].as_str().unwrap();
    assert!(
        msg.contains(":latest") && msg.contains("exceeds the maximum of 5"),
        "{msg}"
    );
}

#[test]
fn validate_lets_deletes_through() {
    let (_, allowed) = validate(&review("DELETE", None)).unwrap();
    assert!(allowed);
}

#[test]
fn admission_rejects_malformed_reviews() {
    assert!(matches!(validate("not json"), Err(Error::Parse(_))));
    let no_request =
        json!({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview"}).to_string();
    for f in [validate, mutate] {
        assert!(matches!(f(&no_request), Err(Error::Parse(m)) if m.contains("no request")));
    }
    assert!(matches!(mutate("{"), Err(Error::Parse(_))));
}

fn apply_patch(object: &Value, response_text: &str) -> Value {
    use base64::Engine;
    let patch_b64 = response(response_text)["patch"]
        .as_str()
        .unwrap()
        .to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(patch_b64)
        .unwrap();
    let patch: json_patch::Patch = serde_json::from_slice(&bytes).unwrap();
    let mut doc = object.clone();
    json_patch::patch(&mut doc, &patch).unwrap();
    doc
}

#[test]
fn mutate_adds_label_and_replicas_as_a_base64_json_patch() {
    let obj = website_obj(json!({"image": "nginx:1.27"}), None);
    let (text, allowed) = mutate(&review("CREATE", Some(obj.clone()))).unwrap();
    assert!(allowed);
    assert_eq!(response(&text)["patchType"], "JSONPatch");
    let after = apply_patch(&obj, &text);
    assert_eq!(
        after["metadata"]["labels"][admission::MANAGED_LABEL],
        "opctl"
    );
    assert_eq!(after["spec"]["replicas"], 1);
    assert_eq!(
        after["spec"]["image"], "nginx:1.27",
        "unrelated fields survive"
    );
}

#[test]
fn mutate_keeps_existing_labels_and_replicas() {
    let obj = website_obj(json!({"image": "nginx:1.27"}), Some(json!({"team": "web"})));
    let (text, _) = mutate(&review("CREATE", Some(obj.clone()))).unwrap();
    let after = apply_patch(&obj, &text);
    assert_eq!(
        after["metadata"]["labels"]["team"], "web",
        "other labels are kept"
    );
    assert_eq!(
        after["metadata"]["labels"][admission::MANAGED_LABEL],
        "opctl"
    );

    let explicit = website_obj(
        json!({"image": "x:1", "replicas": 4}),
        Some(json!({admission::MANAGED_LABEL: "someone-else"})),
    );
    let (text, allowed) = mutate(&review("UPDATE", Some(explicit))).unwrap();
    assert!(allowed);
    assert!(
        response(&text).get("patch").is_none(),
        "nothing to change means no patch"
    );
}

#[test]
fn mutate_creates_missing_spec_and_ignores_deletes() {
    let ops = mutation_ops(&json!({"metadata": {"labels": {admission::MANAGED_LABEL: "x"}}}));
    assert_eq!(
        ops,
        [json!({"op": "add", "path": "/spec", "value": {"replicas": 1}})]
    );
    let (text, allowed) = mutate(&review("DELETE", None)).unwrap();
    assert!(allowed && response(&text).get("patch").is_none());
}

// ---------- CLI ----------

const SITE_YAML: &str = "apiVersion: sites.example.dev/v1\nkind: Website\nmetadata:\n  name: hello\n  namespace: demo\nspec:\n  image: nginx:1.27\n  replicas: 2\n";

#[test]
fn cli_crd_prints_yaml() {
    let (rc, out, _) = run_cli(&["crd"]);
    assert_eq!(rc, 0);
    assert!(out.starts_with("apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition"));
}

#[test]
fn cli_help_and_usage_errors() {
    let (rc, out, _) = run_cli(&["help"]);
    assert_eq!(rc, 0);
    assert!(out.contains("usage: opctl") && out.contains("exit codes"));
    assert_eq!(run_cli(&["--help"]).0, 0);
    for bad in [
        &[][..],
        &["crd", "extra"],
        &["bogus"],
        &["plan"],
        &["validate"],
        &["mutate"],
    ] {
        let (rc, out, err) = run_cli(bad);
        assert_eq!((rc, out.as_str()), (2, ""), "{bad:?}");
        assert!(err.starts_with("opctl: "), "{err}");
    }
}

#[test]
fn cli_plan_prints_actions_as_json() {
    let f = write_tmp("site.yaml", SITE_YAML);
    let (rc, out, _) = run_cli(&["plan", &f]);
    assert_eq!(rc, 0);
    let acts: Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = acts
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["add_finalizer", "create_deployment", "set_status"]);
    assert_eq!(acts[1]["replicas"], 2);
}

#[test]
fn cli_plan_with_a_deployed_state_and_deletion() {
    let f = write_tmp("site2.yaml", SITE_YAML);
    let (rc, out, _) = run_cli(&["plan", &f, "--deployed", "nginx:1.26", "2", "2"]);
    assert_eq!(rc, 0);
    let acts: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(acts[1]["action"], "update_deployment");
    assert_eq!(acts[1]["image"], "nginx:1.27");

    let (rc, out, _) = run_cli(&[
        "plan",
        &f,
        "--deleting",
        "--deployed",
        "nginx:1.27",
        "2",
        "2",
    ]);
    assert_eq!(rc, 0);
    let acts: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        acts,
        json!([{"action": "delete_deployment"}, {"action": "remove_finalizer"}])
    );
}

#[test]
fn cli_plan_rejects_bad_input() {
    let f = write_tmp("site3.yaml", SITE_YAML);
    let junk = write_tmp("junk.yaml", "spec: [");
    assert_eq!(run_cli(&["plan", &junk]).0, 2);
    assert_eq!(run_cli(&["plan", "/nonexistent/site.yaml"]).0, 2);
    assert_eq!(run_cli(&["plan", &f, "--deployed", "img", "two", "1"]).0, 2);
    assert_eq!(run_cli(&["plan", &f, "--deployed", "img", "1"]).0, 2);
    assert_eq!(run_cli(&["plan", &f, "--what"]).0, 2);
}

#[test]
fn cli_validate_and_mutate_exit_codes() {
    let good = write_tmp(
        "ok.json",
        &review(
            "CREATE",
            Some(website_obj(
                json!({"image": "nginx:1.27", "replicas": 2}),
                None,
            )),
        ),
    );
    let bad = write_tmp(
        "bad.json",
        &review(
            "CREATE",
            Some(website_obj(
                json!({"image": "nginx:latest", "replicas": 2}),
                None,
            )),
        ),
    );
    let (rc, out, _) = run_cli(&["validate", &good]);
    assert_eq!(rc, 0);
    assert_eq!(response(&out)["allowed"], true);
    let (rc, out, _) = run_cli(&["validate", &bad]);
    assert_eq!(rc, 1);
    assert_eq!(response(&out)["allowed"], false);
    let (rc, out, _) = run_cli(&["mutate", &good]);
    assert_eq!(rc, 0);
    assert!(response(&out)["patch"].is_string());
    assert_eq!(run_cli(&["validate", "/nonexistent.json"]).0, 2);
    let junk = write_tmp("junk.json", "nope");
    assert_eq!(run_cli(&["mutate", &junk]).0, 2);
}

#[test]
fn run_arguments() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(cli::parse_run_args(&v(&[])), Ok(None));
    assert_eq!(
        cli::parse_run_args(&v(&["--namespace", "demo"])),
        Ok(Some("demo".into()))
    );
    assert!(cli::parse_run_args(&v(&["--namespace"])).is_err());
    assert!(cli::parse_run_args(&v(&["-n", "demo"])).is_err());
}

// ---------- the real binary ----------

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_opctl"))
}

#[test]
fn binary_prints_crd_and_enforces_admission() {
    let o = bin().arg("crd").output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("websites.sites.example.dev"));

    let ok = write_tmp(
        "bin-ok.json",
        &std::fs::read_to_string("examples/review-ok.json").unwrap(),
    );
    let bad = write_tmp(
        "bin-bad.json",
        &std::fs::read_to_string("examples/review-bad.json").unwrap(),
    );
    assert_eq!(
        bin()
            .args(["validate", &ok])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
    let denied = bin().args(["validate", &bad]).output().unwrap();
    assert_eq!(denied.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&denied.stdout).contains("exceeds the maximum of 5"));
    assert_eq!(
        bin().arg("nonsense").output().unwrap().status.code(),
        Some(2)
    );
}

#[test]
fn binary_run_fails_cleanly_without_a_cluster() {
    let o = bin()
        .args(["run", "--namespace", "demo"])
        .env("KUBECONFIG", "/nonexistent/kubeconfig")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot reach a cluster"));
    let bad = bin().args(["run", "--bogus"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn shipped_examples_parse_and_behave() {
    let (rc, out, _) = run_cli(&["plan", "examples/website.yaml"]);
    assert_eq!(rc, 0);
    assert!(out.contains("create_deployment"));
    assert_eq!(run_cli(&["validate", "examples/review-ok.json"]).0, 0);
    assert_eq!(run_cli(&["validate", "examples/review-bad.json"]).0, 1);
}

struct Broken;
impl std::io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_closed_output_is_an_error_not_a_panic() {
    let site = write_tmp("broken.yaml", SITE_YAML);
    let review_file = write_tmp(
        "broken.json",
        &review("CREATE", Some(website_obj(json!({"image": "a:1"}), None))),
    );
    for argv in [
        vec!["crd"],
        vec!["plan", &site],
        vec!["validate", &review_file],
        vec!["help"],
    ] {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut err = Vec::new();
        assert_eq!(cli::run(&args, &mut Broken, &mut err), 2, "{argv:?}");
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("cannot write output"));
    }
}

#[test]
fn plan_rejects_a_non_numeric_ready_count() {
    let f = write_tmp("site-ready.yaml", SITE_YAML);
    assert_eq!(run_cli(&["plan", &f, "--deployed", "img", "1", "x"]).0, 2);
}

// ---------- the contract, and proof that it can fail ----------

use opctl::contract::{check, check_with};

fn allow_all(_: &str) -> Result<(String, bool), Error> {
    Ok((r#"{"response":{"allowed":true}}"#.into(), true))
}
fn deny_all(_: &str) -> Result<(String, bool), Error> {
    Ok((r#"{"response":{"allowed":false}}"#.into(), false))
}
fn broken(_: &str) -> Result<(String, bool), Error> {
    Err(Error::Parse("handler down".into()))
}
fn patch_response(ops: Value) -> (String, bool) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(ops.to_string());
    (
        json!({"response": {"allowed": true, "patch": b64}}).to_string(),
        true,
    )
}
fn always_same_patch(_: &str) -> Result<(String, bool), Error> {
    Ok(patch_response(json!([
        {"op": "add", "path": "/metadata/labels", "value": {admission::MANAGED_LABEL: "opctl"}},
        {"op": "add", "path": "/spec/replicas", "value": 1}
    ])))
}
fn bad_patch(_: &str) -> Result<(String, bool), Error> {
    Ok(patch_response(
        json!([{"op": "replace", "path": "/nope/nothing", "value": 1}]),
    ))
}

fn why(r: Result<(), Error>) -> String {
    match r {
        Err(Error::Invalid(m)) => m,
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn contract_holds_for_the_real_handlers() {
    assert_eq!(check(), Ok(()));
}

#[test]
fn contract_catches_a_validator_that_admits_everything() {
    assert!(why(check_with(allow_all, mutate)).contains("allowed=true, expected false"));
}

#[test]
fn contract_catches_a_validator_that_refuses_everything() {
    assert!(why(check_with(deny_all, mutate)).contains("allowed=false, expected true"));
}

#[test]
fn contract_catches_mutators_that_do_nothing_repeat_or_break() {
    assert!(why(check_with(validate, allow_all)).contains("left a default unset"));
    assert!(why(check_with(validate, always_same_patch)).contains("not idempotent"));
    assert!(why(check_with(validate, bad_patch)).contains("does not apply"));
}

#[test]
fn contract_surfaces_handler_errors() {
    assert_eq!(
        check_with(broken, mutate),
        Err(Error::Parse("handler down".into()))
    );
    assert_eq!(
        check_with(validate, broken),
        Err(Error::Parse("handler down".into()))
    );
}

#[test]
fn cli_contract_reports_ok() {
    let (rc, out, _) = run_cli(&["contract"]);
    assert_eq!(rc, 0);
    assert_eq!(
        out.trim(),
        "contract: admission-refuses-unpinned-or-oversized OK"
    );
    assert_eq!(run_cli(&["contract", "extra"]).0, 2);
}

#[test]
fn binary_contract_line() {
    let o = bin().arg("contract").output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout)
        .contains("contract: admission-refuses-unpinned-or-oversized OK"));
}
