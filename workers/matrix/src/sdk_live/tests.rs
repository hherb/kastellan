use super::*;
use std::collections::HashMap;

fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn parse_config_reads_required_and_defaults() {
    let cfg = parse_config(getter(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m.example.org"),
        ("KASTELLAN_MATRIX_USER", "@bot:m.example.org"),
        ("KASTELLAN_MATRIX_PASSWORD", "hunter2"),
        ("KASTELLAN_MATRIX_STORE", "/var/lib/k/matrix"),
    ]))
    .expect("required vars present");
    assert_eq!(cfg.homeserver_url, "https://m.example.org");
    assert_eq!(cfg.user, "@bot:m.example.org");
    assert_eq!(cfg.store_dir, PathBuf::from("/var/lib/k/matrix"));
    assert_eq!(cfg.device_name, DEVICE_NAME_DEFAULT);
    assert_eq!(cfg.proxy_uds, None);
    assert_eq!(cfg.password.as_deref(), Some("hunter2"));
}

#[test]
fn parse_config_password_is_optional() {
    // No password: valid (a restart restoring a persisted session needs none).
    let cfg = parse_config(getter(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m.example.org"),
        ("KASTELLAN_MATRIX_USER", "bot"),
        ("KASTELLAN_MATRIX_STORE", "/store"),
    ]))
    .expect("password is optional");
    assert_eq!(cfg.password, None);
}

#[test]
fn parse_config_threads_optional_proxy_and_device_name() {
    let cfg = parse_config(getter(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m.example.org"),
        ("KASTELLAN_MATRIX_USER", "bot"),
        ("KASTELLAN_MATRIX_PASSWORD", "pw"),
        ("KASTELLAN_MATRIX_STORE", "/store"),
        ("KASTELLAN_MATRIX_DEVICE_NAME", "kastellan-dgx"),
        ("KASTELLAN_EGRESS_PROXY_UDS", "/run/egress.sock"),
    ]))
    .expect("all vars present");
    assert_eq!(cfg.device_name, "kastellan-dgx");
    assert_eq!(cfg.proxy_uds, Some(PathBuf::from("/run/egress.sock")));
}

#[test]
fn parse_config_fails_closed_on_missing_required() {
    let err = parse_config(getter(&[("KASTELLAN_MATRIX_USER", "bot")])).unwrap_err();
    assert!(err.to_string().contains("KASTELLAN_MATRIX_HOMESERVER_URL"));
}

#[test]
fn drain_returns_all_and_empties() {
    let buf = Mutex::new(VecDeque::new());
    buf.lock().unwrap().push_back(Event {
        conversation: "!r:s".into(),
        peer: "@p:s".into(),
        body: "one".into(),
    });
    buf.lock().unwrap().push_back(Event {
        conversation: "!r:s".into(),
        peer: "@p:s".into(),
        body: "two".into(),
    });
    let drained = drain(&buf);
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].body, "one");
    assert!(drain(&buf).is_empty(), "second drain sees an empty buffer");
}

#[test]
fn initial_live_true_when_prior_token_present() {
    // A persisted sync token means this is a restart: the catch-up sync is
    // incremental (only events since the last run), so we must surface them.
    assert!(initial_live_state(Some("s12_34_56")));
}

#[test]
fn initial_live_false_when_no_prior_token() {
    // No token means a fresh login: the catch-up sync replays room history,
    // which must stay suppressed.
    assert!(!initial_live_state(None));
}

#[test]
fn initial_live_false_when_token_empty() {
    // An empty string is not a real sync position; treat it as no token so a
    // fresh login can never replay full room history.
    assert!(!initial_live_state(Some("")));
}
