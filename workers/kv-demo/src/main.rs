//! kv-demo: a minimal LONG-LIVED `Net::Deny` worker — a tiny persistent
//! key/value store over JSON-RPC stdio. It exists to exercise slice 5b's
//! persistent-VM lifecycle: it serves many calls over one boot (`kv.stats`
//! proves liveness) and its store survives a respawn (`kv.put`/`kv.get` against
//! the persistent RW mount). Store dir comes from `KASTELLAN_KV_STORE_DIR`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::serve_stdio;
use serde::Deserialize;

#[derive(Deserialize)]
struct PutParams { key: String, value: String }
#[derive(Deserialize)]
struct GetParams { key: String }

struct KvHandler { dir: PathBuf, calls_served: u64 }

impl KvHandler {
    fn new(dir: PathBuf) -> Self { Self { dir, calls_served: 0 } }
    fn store_path(&self) -> PathBuf { self.dir.join("store.json") }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read(self.store_path()).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save(&self, map: &BTreeMap<String, String>) -> Result<(), RpcError> {
        let tmp = self.dir.join("store.json.tmp");
        let bytes = serde_json::to_vec(map)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("serialize store: {e}")))?;
        std::fs::write(&tmp, &bytes)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("write store: {e}")))?;
        std::fs::rename(&tmp, self.store_path())
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("rename store: {e}")))?;
        Ok(())
    }
}

impl Handler for KvHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        self.calls_served += 1;
        match method {
            "kv.put" => {
                let p: PutParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let mut map = self.load();
                map.insert(p.key, p.value);
                self.save(&map)?;
                Ok(serde_json::json!({ "ok": true }))
            }
            "kv.get" => {
                let p: GetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let map = self.load();
                Ok(serde_json::json!({ "value": map.get(&p.key) }))
            }
            "kv.stats" => Ok(serde_json::json!({ "calls_served": self.calls_served, "pid": std::process::id() })),
            // kv.crash: deterministic worker-death trigger for lifecycle e2e tests.
            // Calls std::process::exit(1) without sending a reply so the caller
            // receives an I/O error (broken pipe / EOF), which PersistentWorker
            // treats as a worker death and respawns. Only compiled in debug builds.
            #[cfg(debug_assertions)]
            "kv.crash" => std::process::exit(1),
            other => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::var("KASTELLAN_KV_STORE_DIR")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("KASTELLAN_KV_STORE_DIR must be set"))?;
    let mut handler = KvHandler::new(dir);
    serve_stdio(&mut handler)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handler_in(dir: &std::path::Path) -> KvHandler {
        KvHandler::new(dir.to_path_buf())
    }
    #[test]
    fn put_then_get_round_trips_and_persists() {
        let dir = std::env::temp_dir().join(format!("kvdemo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        h.call("kv.put", serde_json::json!({"key":"a","value":"1"})).unwrap();
        // a fresh handler reading the same dir sees the persisted value
        let mut h2 = handler_in(&dir);
        let got = h2.call("kv.get", serde_json::json!({"key":"a"})).unwrap();
        assert_eq!(got["value"], "1");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn stats_counts_calls() {
        let dir = std::env::temp_dir().join(format!("kvdemo-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        h.call("kv.put", serde_json::json!({"key":"a","value":"1"})).unwrap();
        h.call("kv.get", serde_json::json!({"key":"a"})).unwrap();
        let s = h.call("kv.stats", serde_json::json!({})).unwrap();
        assert_eq!(s["calls_served"], 3); // put + get + stats
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn get_missing_key_returns_null_value() {
        let dir = std::env::temp_dir().join(format!("kvdemo-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        let got = h.call("kv.get", serde_json::json!({"key":"nope"})).unwrap();
        assert!(got["value"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }
}
