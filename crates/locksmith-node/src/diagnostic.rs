use locksmith::{ConflictKind, DocumentKind, LockfileError, PeerIssue, RegistryRequest, Value};
use napi::{Env, Error, JsValue, bindgen_prelude::JsObjectValue};
use serde_json::json;

pub(super) fn peers(issues: Vec<PeerIssue>) -> Vec<Value> {
    issues
        .into_iter()
        .map(|issue| {
            json!({
                "snapshot": issue.snapshot, "peer": issue.peer,
                "wanted": issue.wanted, "resolved": issue.resolved,
            })
        })
        .collect()
}

fn document(kind: DocumentKind) -> &'static str {
    match kind {
        DocumentKind::Project => "project",
        DocumentKind::Environment => "environment",
    }
}

pub(super) fn exception(env: &Env, error: LockfileError) -> napi::Result<Error> {
    let mut object = env.create_error(Error::from_reason(error.to_string()))?;
    let (code, details) = match error {
        LockfileError::ResolutionRequired {
            path,
            message,
            request,
        } => {
            let request = request.map(|request| match *request {
                RegistryRequest::Version { name, selector } => {
                    json!({"kind":"version","name":name,"selector":selector})
                }
                RegistryRequest::Manifest { name, version } => {
                    json!({"kind":"manifest","name":name,"version":version})
                }
            });
            (
                "RESOLUTION_REQUIRED",
                json!({"path":path,"reason":message,"request":request}),
            )
        }
        LockfileError::Conflicts(conflicts) => {
            let conflicts: Vec<_> = conflicts.into_iter().map(|conflict| {
                let kind = match conflict.kind {
                    ConflictKind::BothChanged => "bothChanged",
                    ConflictKind::DeleteModify => "deleteModify",
                    ConflictKind::SharedDependency => "sharedDependency",
                };
                json!({
                    "kind": kind, "document": document(conflict.document), "path": conflict.path,
                    "base": conflict.base, "ours": conflict.ours, "theirs": conflict.theirs,
                })
            }).collect();
            ("CONFLICTS", json!({ "conflicts": conflicts }))
        }
        LockfileError::InvalidGraph(issues) => {
            let issues: Vec<_> = issues.into_iter().map(|issue| {
                json!({ "document": document(issue.document), "path": issue.path, "message": issue.message })
            }).collect();
            ("INVALID_GRAPH", json!({ "issues": issues }))
        }
        LockfileError::UnmetPeers(issues) => ("UNMET_PEERS", json!({"issues":peers(issues)})),
        LockfileError::InvalidManifest(importer) => {
            ("INVALID_MANIFEST", json!({"importer":importer}))
        }
        LockfileError::ManifestMismatch {
            importer,
            side,
            message,
        } => (
            "MANIFEST_MISMATCH",
            json!({"importer":importer,"side":side,"reason":message}),
        ),
        LockfileError::GlobalConfigurationChanged { importer, field } => (
            "GLOBAL_CONFIGURATION_CHANGED",
            json!({"importer":importer,"field":field}),
        ),
        LockfileError::UnsupportedVersion { version } => {
            ("UNSUPPORTED_VERSION", json!({"version":version}))
        }
        LockfileError::Parse(_) => ("INVALID_YAML", Value::Null),
        LockfileError::InvalidDocument(_) => ("INVALID_DOCUMENT", Value::Null),
        LockfileError::WorkspaceInput(_) => ("WORKSPACE_INPUT", Value::Null),
        LockfileError::Serialize(_) => ("SERIALIZATION_FAILED", Value::Null),
        LockfileError::OutputMismatch => ("OUTPUT_MISMATCH", Value::Null),
    };
    object.set_named_property("code", code)?;
    object.set_named_property("details", details)?;
    Ok(object.to_unknown().into())
}
