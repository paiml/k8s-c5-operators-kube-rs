//! The admission contract, checked on a matrix of inputs through the handlers.
//! See `contracts/admission-v1.yaml`.
use crate::admission::{mutate, validate, MANAGED_LABEL};
use crate::Error;
use base64::Engine;
use serde_json::{json, Value};

/// A webhook handler: review JSON in, (response JSON, allowed) out.
pub type Handler = fn(&str) -> Result<(String, bool), Error>;

/// (image, is it pinned?) pairs the matrix is built from. The truth column is
/// written by hand here, independently of `image_problem`.
const IMAGES: [(&str, bool); 6] = [
    ("nginx:1.27", true),
    ("localhost:5000/team/app:v2", true),
    ("ghcr.io/o/app@sha256:0123abcdef", true),
    ("nginx:latest", false),
    ("nginx", false),
    ("", false),
];

const REPLICAS: [Option<i64>; 7] = [None, Some(-1), Some(0), Some(1), Some(5), Some(6), Some(9)];

fn review(object: &Value) -> String {
    json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "contract",
            "kind": {"group": "sites.example.dev", "version": "v1", "kind": "Website"},
            "resource": {"group": "sites.example.dev", "version": "v1", "resource": "websites"},
            "operation": "CREATE",
            "userInfo": {},
            "object": object
        }
    })
    .to_string()
}

fn website(image: &str, replicas: Option<i64>) -> Value {
    let mut spec = json!({ "image": image });
    if let Some(r) = replicas {
        spec["replicas"] = json!(r);
    }
    json!({
        "apiVersion": "sites.example.dev/v1",
        "kind": "Website",
        "metadata": {"name": "w", "namespace": "n"},
        "spec": spec
    })
}

fn decode_patch(response: &str) -> Option<json_patch::Patch> {
    let value: Value = serde_json::from_str(response).expect("handlers emit JSON");
    let b64 = value["response"]["patch"].as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("the patch is base64");
    Some(serde_json::from_slice(&bytes).expect("the patch is a JSON patch"))
}

fn violated(what: String) -> Error {
    Error::Invalid(format!("contract violated: {what}"))
}

/// Checks both invariants with the real handlers.
pub fn check() -> Result<(), Error> {
    check_with(validate, mutate)
}

/// Checks both invariants with the given handlers; `Err` names the first counterexample.
///
/// 1. A Website is admitted exactly when its image is pinned and replicas is in 0..=5.
/// 2. Mutating an admitted Website sets the default label and replicas, and
///    mutating the result again changes nothing.
pub fn check_with(validator: Handler, mutator: Handler) -> Result<(), Error> {
    for (image, pinned) in IMAGES {
        for replicas in REPLICAS {
            let object = website(image, replicas);
            let (_, allowed) = validator(&review(&object))?;
            let expected = pinned && replicas.is_none_or(|r| (0..=5).contains(&r));
            if allowed != expected {
                return Err(violated(format!(
                    "validate({image:?}, {replicas:?}) allowed={allowed}, expected {expected}"
                )));
            }
            if !expected {
                continue;
            }
            let (text, _) = mutator(&review(&object))?;
            let mut patched = object.clone();
            if let Some(patch) = decode_patch(&text) {
                if json_patch::patch(&mut patched, &patch).is_err() {
                    return Err(violated(format!(
                        "mutate({image:?}, {replicas:?}) produced a patch that does not apply"
                    )));
                }
            }
            if patched["metadata"]["labels"][MANAGED_LABEL].is_null()
                || patched["spec"]["replicas"].is_null()
            {
                return Err(violated(format!(
                    "mutate({image:?}, {replicas:?}) left a default unset"
                )));
            }
            let (again, _) = mutator(&review(&patched))?;
            if decode_patch(&again).is_some() {
                return Err(violated(format!(
                    "mutate({image:?}, {replicas:?}) is not idempotent"
                )));
            }
        }
    }
    Ok(())
}
