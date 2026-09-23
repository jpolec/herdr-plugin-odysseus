//! Schema migrations for persisted documents.
//!
//! Every persisted document carries `schema_version`. When the binary reads
//! an older document, [`migrate`] upgrades it step by step and the caller
//! backs up the original first. Version 1 is the first released schema, so
//! the registry is currently empty except for the v0 → v1 rule used for
//! pre-release state (documents written without a few later-added fields).

use anyhow::{bail, Result};
use serde_json::Value;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

pub struct Migration {
    pub kind: &'static str,
    pub from: u32,
    pub apply: fn(Value) -> Result<Value>,
}

fn v0_to_v1_run(mut v: Value) -> Result<Value> {
    if let Some(o) = v.as_object_mut() {
        o.entry("retry_counts").or_insert_with(|| Value::Object(Default::default()));
        o.entry("artifacts").or_insert_with(|| Value::Array(vec![]));
    }
    Ok(v)
}

fn identity(v: Value) -> Result<Value> {
    Ok(v)
}

const MIGRATIONS: &[Migration] = &[
    Migration { kind: "run", from: 0, apply: v0_to_v1_run },
    Migration { kind: "task", from: 0, apply: identity },
    Migration { kind: "approval", from: 0, apply: identity },
    Migration { kind: "control", from: 0, apply: identity },
    Migration { kind: "scheduler", from: 0, apply: identity },
];

pub fn migrate(kind: &str, mut from: u32, mut data: Value) -> Result<Value> {
    while from < CURRENT_SCHEMA_VERSION {
        let Some(m) = MIGRATIONS.iter().find(|m| m.kind == kind && m.from == from) else {
            bail!("no migration for {kind} from schema v{from}");
        };
        data = (m.apply)(data)?;
        from += 1;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{read_doc, sha256_hex, StateLayout};

    #[test]
    fn migrates_v0_run_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(dir.path());
        layout.ensure().unwrap();
        let data = serde_json::json!({"x": 1});
        let env = serde_json::json!({
            "schema_version": 0,
            "kind": "run",
            "sha256": sha256_hex(crate::store::canonical_json(&data).as_bytes()),
            "data": data,
        });
        let p = dir.path().join("state/runs/r.json");
        std::fs::write(&p, serde_json::to_vec(&env).unwrap()).unwrap();
        let v: Value = read_doc(&layout, &p, "run").unwrap();
        assert!(v.get("retry_counts").is_some());
        assert_eq!(std::fs::read_dir(layout.backups_dir()).unwrap().count(), 1);
        // Rewritten at the current version.
        let raw: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(raw["schema_version"], CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_kind_fails() {
        assert!(migrate("nope", 0, Value::Null).is_err());
    }
}
