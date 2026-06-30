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

    /// Load the store. A genuinely absent file is an empty store (legitimate
    /// first run). Any other error — unreadable file, or a corrupt/truncated
    /// JSON body — fails CLOSED: returning an empty map here would make the very
    /// next `kv.put` (load → insert → save) persist a one-entry map and silently
    /// wipe every previously-stored key. The caller must surface the error
    /// instead of destroying state on a transient read or partial write.
    fn load(&self) -> Result<BTreeMap<String, String>, RpcError> {
        let path = self.store_path();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                RpcError::new(codes::OPERATION_FAILED, format!("corrupt store {path:?}: {e}"))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(RpcError::new(
                codes::OPERATION_FAILED,
                format!("read store {path:?}: {e}"),
            )),
        }
    }
    fn save(&self, map: &BTreeMap<String, String>) -> Result<(), RpcError> {
        use std::io::Write;
        let tmp = self.dir.join("store.json.tmp");
        let bytes = serde_json::to_vec(map)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("serialize store: {e}")))?;
        // Write + fsync the temp file BEFORE the rename so its bytes are durable
        // on the persistent device; otherwise a VM SIGKILL between rename and
        // flush could leave the renamed file pointing at unwritten data, and a
        // `kv.put` that returned ok:true would lose its value across a respawn.
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| {
                RpcError::new(codes::OPERATION_FAILED, format!("create store tmp: {e}"))
            })?;
            f.write_all(&bytes)
                .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("write store: {e}")))?;
            f.sync_all()
                .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("fsync store: {e}")))?;
        }
        std::fs::rename(&tmp, self.store_path())
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("rename store: {e}")))?;
        // fsync the directory so the rename (a metadata op) is itself durable.
        // Best-effort: not every filesystem supports directory fsync, and the
        // data+rename ordering above is the load-bearing guarantee.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
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
                let mut map = self.load()?;
                map.insert(p.key, p.value);
                self.save(&map)?;
                Ok(serde_json::json!({ "ok": true }))
            }
            "kv.get" => {
                let p: GetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let map = self.load()?;
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

    #[test]
    fn corrupt_store_fails_closed_and_does_not_wipe() {
        // A corrupt/truncated store.json must error rather than silently load an
        // empty map (which the next kv.put would persist, destroying all keys).
        let dir = std::env::temp_dir().join(format!("kvdemo-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Seed a valid value, then corrupt the file on disk.
        let mut h = handler_in(&dir);
        h.call("kv.put", serde_json::json!({"key":"keep","value":"v"})).unwrap();
        std::fs::write(dir.join("store.json"), b"{ this is not json").unwrap();
        // get and put both fail closed.
        assert!(h.call("kv.get", serde_json::json!({"key":"keep"})).is_err());
        assert!(h.call("kv.put", serde_json::json!({"key":"new","value":"x"})).is_err());
        // The corrupt file is left intact (not overwritten with a 1-entry map).
        let raw = std::fs::read(dir.join("store.json")).unwrap();
        assert_eq!(raw, b"{ this is not json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_store_loads_empty_not_error() {
        // A genuinely absent store is a legitimate first run → empty, not an error.
        let dir = std::env::temp_dir().join(format!("kvdemo-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = handler_in(&dir);
        assert!(h.load().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
