//! Version-aware, byte-preserving codecs for future SQLite v2 reads/audits.
//! No database is opened or migrated here. Legacy receipts must continue to use
//! original v1 values, NEVER projected v2 snapshots, for historical prefix parity.
use super::*;
use crate::execution::{self as v1, validation as old};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

fn invalid() -> v1::Error {
    old::error(
        "storage_unavailable",
        "Stored execution data could not be validated.",
    )
}

fn stored_version(raw: &str, limit: usize) -> DomainResult<u64> {
    if raw.len() > limit {
        return Err(invalid());
    }
    #[derive(serde::Deserialize)]
    struct Version {
        contract_version: u64,
        kind: Option<String>,
    }
    let version: Version = serde_json::from_str(raw).map_err(|_| invalid())?;
    if limit == EVENT_BYTES
        && version.kind.as_deref() != Some("text_delta")
        && raw.len() > NONTEXT_EVENT_BYTES
    {
        return Err(invalid());
    }
    if !matches!(version.contract_version, 1 | 2) {
        return Err(old::error(
            "unsupported_contract_version",
            "Stored execution version is unsupported and preserved.",
        ));
    }
    Ok(version.contract_version)
}

// Pure read projection. Caller validates the original with v1 rules FIRST.
fn project<T: DeserializeOwned>(value: &impl Serialize) -> DomainResult<T> {
    fn walk(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                if fields.contains_key("contract_version") {
                    fields.insert("contract_version".into(), Value::from(2));
                }
                if fields.contains_key("enqueue_order") && fields.contains_key("last_seq") {
                    fields.insert("profile_id".into(), Value::Null);
                    fields.insert("model_id".into(), Value::Null);
                }
                if fields.contains_key("estimated") && fields.contains_key("input_tokens") {
                    for key in [
                        "cached_input_tokens",
                        "cache_write_input_tokens",
                        "reasoning_output_tokens",
                    ] {
                        fields.insert(key.into(), Value::Null);
                    }
                }
                for item in fields.values_mut() {
                    walk(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let mut json = serde_json::to_value(value).map_err(|_| invalid())?;
    walk(&mut json);
    serde_json::from_value(json).map_err(|_| invalid())
}

macro_rules! stored_record {
    ($name:ident, $versions:ident, $legacy:ty, $current:ty, $v1check:path, $v2check:path, $limit:expr) => {
        #[derive(Clone, PartialEq)]
        pub enum $versions {
            V1($legacy),
            V2($current),
        }
        // No Debug/Serialize: records can carry private assistant output.
        pub struct $name {
            raw: Box<str>,
            value: $versions,
        }
        impl $name {
            pub fn decode(raw: &str) -> DomainResult<Self> {
                let value = match stored_version(raw, $limit)? {
                    1 => {
                        let value: $legacy = serde_json::from_str(raw).map_err(|_| invalid())?;
                        $v1check(&value).map_err(|_| invalid())?;
                        $versions::V1(value)
                    }
                    2 => {
                        let value: $current = serde_json::from_str(raw).map_err(|_| invalid())?;
                        $v2check(&value).map_err(|_| invalid())?;
                        $versions::V2(value)
                    }
                    _ => unreachable!(),
                };
                Ok(Self {
                    raw: raw.into(),
                    value,
                })
            }
            pub fn original_json(&self) -> &str {
                &self.raw
            }
            pub fn original(&self) -> &$versions {
                &self.value
            }
            pub fn projected(&self) -> DomainResult<$current> {
                let result = match &self.value {
                    $versions::V1(value) => project(value)?,
                    $versions::V2(value) => value.clone(),
                };
                $v2check(&result).map_err(|_| invalid())?;
                Ok(result)
            }
        }
    };
}

stored_record!(
    StoredSession,
    SessionVersion,
    v1::SessionSnapshot,
    SessionSnapshot,
    old::validate_session,
    validation::validate_session,
    16_384
);
stored_record!(
    StoredRunSnapshot,
    SnapshotVersion,
    v1::RunSnapshot,
    RunSnapshot,
    old::validate_snapshot,
    validation::validate_snapshot,
    65_536
);
stored_record!(
    StoredRunEvent,
    EventVersion,
    v1::RunEvent,
    RunEvent,
    old::validate_event,
    validation::validate_event,
    EVENT_BYTES
);

/// Preview/read projection only. Existing workspace layout stays version1;
/// availability and execution are explicitly disabled until the new native
/// owner has completed its own reviewed upgrade/recovery/admission gates.
pub fn project_legacy_workspace(value: &v1::WorkspaceSnapshot) -> DomainResult<WorkspaceSnapshot> {
    old::validate_workspace_snapshot(value).map_err(|_| invalid())?;
    let mut json = serde_json::to_value(value).map_err(|_| invalid())?;
    json["codex_availability"] = Value::from("not_configured");
    json["execution_available"] = Value::from(false);
    let result: WorkspaceSnapshot = project(&json)?;
    validation::validate_workspace(&result).map_err(|_| invalid())?;
    Ok(result)
}

/// Compare original-version values at a historical prefix. Projecting first
/// would falsely reject byte-preserved v1 start/cancel receipts after upgrade.
pub fn same_original_prefix(left: &StoredRunSnapshot, right: &StoredRunSnapshot) -> bool {
    left.original() == right.original()
}

/// Version-homogeneous replay for a stored run: legacy recovery stays v1.
/// New v2 events cannot silently extend a v1 run. Both variants use the shared
/// transition reducer through their respective strict policy validators.
pub fn apply_stored(
    snapshot: &StoredRunSnapshot,
    event: &StoredRunEvent,
) -> DomainResult<StoredRunSnapshot> {
    let json = match (snapshot.original(), event.original()) {
        (SnapshotVersion::V1(s), EventVersion::V1(e)) => {
            serde_json::to_string(&old::apply_event(s, e)?)
        }
        (SnapshotVersion::V2(s), EventVersion::V2(e)) => {
            serde_json::to_string(&validation::apply_event(s, e)?)
        }
        _ => return Err(invalid()),
    }
    .map_err(|_| invalid())?;
    StoredRunSnapshot::decode(&json)
}
