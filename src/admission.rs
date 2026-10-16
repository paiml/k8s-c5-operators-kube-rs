//! Validating and mutating admission for Website objects.
use crate::Error;
use base64::Engine;
use json_patch::Patch;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use kube::core::DynamicObject;
use serde_json::{json, Value};

/// Most replicas a Website may ask for.
pub const MAX_REPLICAS: i64 = 5;
/// Label the mutating webhook adds when it is missing.
pub const MANAGED_LABEL: &str = "app.kubernetes.io/managed-by";
/// Value of the managed-by label.
pub const MANAGED_VALUE: &str = "opctl";

/// Why an image reference is not pinned, or `None` when it is.
pub fn image_problem(image: &str) -> Option<String> {
    if image.is_empty() {
        return Some("spec.image must not be empty".into());
    }
    if let Some((_, digest)) = image.split_once('@') {
        let ok = digest.split_once(':').is_some_and(|(algo, hex)| {
            !algo.is_empty() && !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
        });
        return if ok {
            None
        } else {
            Some(format!("spec.image {image} has a malformed digest"))
        };
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    match last.split_once(':') {
        None => Some(format!(
            "spec.image {image} has no tag, which means :latest; pin a version"
        )),
        Some((_, "latest")) => Some(format!("spec.image {image} uses :latest; pin a version")),
        Some((_, "")) => Some(format!("spec.image {image} has an empty tag")),
        Some(_) => None,
    }
}

/// Every rule a Website spec breaks, in a stable order.
pub fn violations(object: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let spec = &object["spec"];
    match spec["image"].as_str() {
        None => out.push("spec.image is required".to_string()),
        Some(image) => out.extend(image_problem(image)),
    }
    match &spec["replicas"] {
        Value::Null => {}
        v => match v.as_i64() {
            None => out.push("spec.replicas must be an integer".to_string()),
            Some(n) if n > MAX_REPLICAS => out.push(format!(
                "spec.replicas {n} exceeds the maximum of {MAX_REPLICAS}"
            )),
            Some(n) if n < 0 => out.push(format!("spec.replicas {n} must not be negative")),
            Some(_) => {}
        },
    }
    out
}

/// Parses a review and extracts its request.
fn request(text: &str) -> Result<AdmissionRequest<DynamicObject>, Error> {
    let review: AdmissionReview<DynamicObject> =
        serde_json::from_str(text).map_err(|e| Error::Parse(e.to_string()))?;
    review
        .try_into()
        .map_err(|_| Error::Parse("AdmissionReview has no request".into()))
}

/// The object under review as plain JSON, if the request carries one.
fn object_value(req: &AdmissionRequest<DynamicObject>) -> Option<Value> {
    req.object
        .as_ref()
        .map(|o| serde_json::to_value(o).expect("a parsed object serialises back to JSON"))
}

/// Renders the response review. kube-core serialises the patch as a byte
/// array, but the API server expects base64, so the patch is re-encoded here.
fn render(response: AdmissionResponse) -> (String, bool) {
    let allowed = response.allowed;
    let patch = response.patch.clone();
    let mut value =
        serde_json::to_value(response.into_review()).expect("a response serialises to JSON");
    if let Some(bytes) = patch {
        value["response"]["patch"] =
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    (format!("{value:#}"), allowed)
}

/// Validating webhook: refuses replicas above 5 and images that are not pinned.
/// Returns the response review as JSON and whether the request was allowed.
pub fn validate(text: &str) -> Result<(String, bool), Error> {
    let req = request(text)?;
    let mut response = AdmissionResponse::from(&req);
    if let Some(object) = object_value(&req) {
        let found = violations(&object);
        if !found.is_empty() {
            response = response.deny(found.join("; "));
            response.result.code = 403;
            response.result.reason = "Forbidden".into();
        }
    }
    Ok(render(response))
}

/// The JSON patch operations that add the default label and the default replicas.
pub fn mutation_ops(object: &Value) -> Vec<Value> {
    let mut ops = Vec::new();
    match object["metadata"]["labels"].as_object() {
        None => ops.push(json!({
            "op": "add", "path": "/metadata/labels", "value": { MANAGED_LABEL: MANAGED_VALUE }
        })),
        Some(labels) if !labels.contains_key(MANAGED_LABEL) => ops.push(json!({
            "op": "add",
            "path": format!("/metadata/labels/{}", MANAGED_LABEL.replace('/', "~1")),
            "value": MANAGED_VALUE
        })),
        Some(_) => {}
    }
    if object["spec"].is_null() {
        ops.push(json!({
            "op": "add", "path": "/spec", "value": { "replicas": crate::reconcile::DEFAULT_REPLICAS }
        }));
    } else if object["spec"]["replicas"].is_null() {
        ops.push(json!({
            "op": "add", "path": "/spec/replicas", "value": crate::reconcile::DEFAULT_REPLICAS
        }));
    }
    ops
}

/// Mutating webhook: adds the default label and defaults replicas with a JSON patch.
pub fn mutate(text: &str) -> Result<(String, bool), Error> {
    let req = request(text)?;
    let mut response = AdmissionResponse::from(&req);
    if let Some(object) = object_value(&req) {
        let ops = mutation_ops(&object);
        if !ops.is_empty() {
            let patch: Patch = serde_json::from_value(Value::Array(ops))
                .expect("mutation_ops builds valid patch operations");
            response = response
                .with_patch(patch)
                .expect("a patch serialises to JSON");
        }
    }
    Ok(render(response))
}
