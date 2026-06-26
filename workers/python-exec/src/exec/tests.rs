use super::*;

/// The flag set is a containment decision (curated stdlib, env
/// isolation, stdin delivery) — pin it so a change is deliberate.
#[test]
fn python_args_pin_isolated_no_site_no_pyc_stdin() {
    assert_eq!(python_args(), ["-I", "-S", "-B", "-"]);
}

#[test]
fn truncate_lossy_passes_small_input_through() {
    let (s, t) = truncate_lossy(b"hello", 16);
    assert_eq!(s, "hello");
    assert!(!t);
}

#[test]
fn truncate_lossy_caps_at_exact_boundary_without_flag() {
    let (s, t) = truncate_lossy(b"abcd", 4);
    assert_eq!(s, "abcd");
    assert!(!t);
}

#[test]
fn truncate_lossy_never_splits_a_multibyte_char() {
    // "é" is 2 bytes in UTF-8; a 3-byte cap on "aéb" must cut before
    // the 'b' lands but also must not split 'é'.
    let bytes = "aéb".as_bytes(); // [0x61, 0xC3, 0xA9, 0x62]
    let (s, t) = truncate_lossy(bytes, 2);
    assert_eq!(s, "a");
    assert!(t);
    let (s3, t3) = truncate_lossy(bytes, 3);
    assert_eq!(s3, "aé");
    assert!(t3);
}

#[test]
fn truncate_lossy_handles_invalid_utf8_lossily() {
    let (s, t) = truncate_lossy(&[0x61, 0xFF, 0x62], 64);
    assert_eq!(s, "a\u{FFFD}b");
    assert!(!t);
}

#[test]
fn read_capped_under_cap_returns_everything_unflagged() {
    let (bytes, truncated) = read_capped(std::io::Cursor::new(b"hello".to_vec()), 16).unwrap();
    assert_eq!(bytes, b"hello");
    assert!(!truncated);
}

#[test]
fn read_capped_over_cap_keeps_prefix_drains_rest_and_flags() {
    // 100 KiB source, 4 KiB cap: the buffer must hold exactly the
    // first 4 KiB (multiple-chunk path), the flag must be set, and
    // the read must run to EOF (drain) rather than stopping at cap.
    let data: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
    let cap = 4 * 1024;
    let (bytes, truncated) = read_capped(std::io::Cursor::new(data.clone()), cap).unwrap();
    assert_eq!(bytes, &data[..cap]);
    assert!(truncated);
}

#[test]
fn read_capped_at_exact_cap_is_unflagged() {
    let data = vec![7u8; 64];
    let (bytes, truncated) = read_capped(std::io::Cursor::new(data.clone()), 64).unwrap();
    assert_eq!(bytes, data);
    assert!(!truncated);
}

#[test]
fn serialize_params_none_is_empty_object() {
    assert_eq!(serialize_params(&None).unwrap(), "{}");
}

#[test]
fn serialize_params_object_round_trips() {
    let v = serde_json::json!({"a": 1, "b": "x"});
    let s = serialize_params(&Some(v)).unwrap();
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back, serde_json::json!({"a": 1, "b": "x"}));
}

#[test]
fn serialize_params_rejects_non_object() {
    assert!(matches!(
        serialize_params(&Some(serde_json::json!([1, 2]))),
        Err(ParamsError::NotObject)
    ));
    assert!(matches!(
        serialize_params(&Some(serde_json::json!("flat"))),
        Err(ParamsError::NotObject)
    ));
    assert!(matches!(
        serialize_params(&Some(serde_json::Value::Null)),
        Err(ParamsError::NotObject)
    ));
}

#[test]
fn serialize_params_allows_newlines_in_values() {
    let v = serde_json::json!({ "text": "line1\nline2" });
    let s = serialize_params(&Some(v)).unwrap();
    assert!(!s.contains('\n'), "raw newline must be escaped inside JSON");
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back["text"], "line1\nline2");
}

#[test]
fn serialize_params_escapes_nul_no_raw_nul_for_execve() {
    // The serialized string becomes a single C-string env value handed to
    // `execve`; a raw NUL would silently truncate it. serde escapes NUL
    // as the 6-char sequence \u0000, so the output must contain no raw
    // NUL byte and must still round-trip to the original value.
    let v = serde_json::json!({ "text": "a\u{0000}b" });
    let s = serialize_params(&Some(v)).unwrap();
    assert!(!s.as_bytes().contains(&0), "serialized params must be NUL-free");
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back["text"], "a\u{0000}b");
}

#[test]
fn scratch_dir_defaults_to_tmp_when_unset() {
    let s = scratch_dir_from_env(|_| None);
    assert_eq!(s, "/tmp");
}

#[test]
fn scratch_dir_uses_env_when_set() {
    let s = scratch_dir_from_env(|k| {
        (k == WORKER_SCRATCH_ENV).then(|| "/var/folders/xx/pyexec-1-1".to_string())
    });
    assert_eq!(s, "/var/folders/xx/pyexec-1-1");
}

#[test]
fn scratch_dir_falls_back_when_env_is_empty() {
    let s = scratch_dir_from_env(|_| Some(String::new()));
    assert_eq!(s, "/tmp");
}

#[test]
fn params_file_max_defaults_when_unset() {
    let m = params_file_max(|_| None);
    assert_eq!(m, PARAMS_FILE_MAX_DEFAULT);
}

#[test]
fn params_file_max_parses_a_valid_value() {
    let m = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "200000".to_string()));
    assert_eq!(m, 200_000);
}

#[test]
fn params_file_max_garbage_falls_back_to_default() {
    let m = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "not-a-number".to_string()));
    assert_eq!(m, PARAMS_FILE_MAX_DEFAULT);
}

#[test]
fn params_file_max_clamps_below_inline_and_above_abs() {
    // Below the inline threshold is nonsensical (file channel only fires
    // above inline) → clamp up.
    let low = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "1".to_string()));
    assert_eq!(low, INLINE_PARAMS_MAX);
    // Above the absolute ceiling → clamp down.
    let high = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "999999999".to_string()));
    assert_eq!(high, PARAMS_FILE_MAX_ABS);
}

#[test]
fn decide_inline_at_and_below_threshold() {
    assert_eq!(
        decide_param_channel(0, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
        ParamChannel::Inline
    );
    assert_eq!(
        decide_param_channel(INLINE_PARAMS_MAX, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
        ParamChannel::Inline
    );
}

#[test]
fn decide_file_just_over_inline_and_at_ceiling() {
    assert_eq!(
        decide_param_channel(INLINE_PARAMS_MAX + 1, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
        ParamChannel::File
    );
    assert_eq!(
        decide_param_channel(PARAMS_FILE_MAX_DEFAULT, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
        ParamChannel::File
    );
}

#[test]
fn decide_too_large_over_ceiling() {
    let err = decide_param_channel(
        PARAMS_FILE_MAX_DEFAULT + 1, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT,
    )
    .unwrap_err();
    assert!(matches!(err, ParamsError::TooLarge { .. }));
}

#[test]
fn params_env_pairs_inline_sets_only_params_env() {
    let pairs = params_env_pairs(&ParamChannel::Inline, r#"{"a":1}"#, "/unused");
    assert_eq!(pairs, vec![(PARAMS_ENV, r#"{"a":1}"#.to_string())]);
}

#[test]
fn params_env_pairs_file_sets_empty_default_plus_path() {
    let pairs = params_env_pairs(&ParamChannel::File, r#"{"a":1}"#, "/tmp/params.json");
    assert_eq!(
        pairs,
        vec![
            (PARAMS_ENV, "{}".to_string()),
            (PARAMS_FILE_ENV, "/tmp/params.json".to_string()),
        ]
    );
}

/// Build a unique, freshly-created temp dir for a wipe test (the crate has no
/// `tempfile` dev-dep; this mirrors `write_params_file`'s pattern below).
fn fresh_tmp(tag: u32) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pyexec-wipe-test-{}-{}",
        std::process::id(),
        tag
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn wipe_scratch_contents_removes_files_and_subdirs_keeps_dir() {
    let root = fresh_tmp(line!());
    std::fs::write(root.join("params.json"), b"{}").unwrap();
    std::fs::write(root.join("leak.txt"), b"secret").unwrap();
    let sub = root.join("nested");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("inner.bin"), b"x").unwrap();

    let removed = wipe_scratch_contents(&root).expect("wipe ok");

    assert_eq!(removed, 3, "params.json + leak.txt + nested/ are 3 top-level entries");
    assert!(root.is_dir(), "the scratch dir itself must remain");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "scratch dir must be empty after wipe"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn wipe_scratch_contents_is_noop_on_empty_dir() {
    let root = fresh_tmp(line!());
    let removed = wipe_scratch_contents(&root).expect("wipe ok");
    assert_eq!(removed, 0, "empty dir -> nothing removed (the fresh-VM no-op case)");
    assert!(root.is_dir());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn wipe_scratch_contents_missing_dir_is_ok_zero() {
    // A not-yet-created scratch dir must not error — run_code tolerates it
    // (it only sets cwd when the dir exists). Treat absent as "nothing to wipe".
    let root = fresh_tmp(line!());
    let missing = root.join("does-not-exist");
    let removed = wipe_scratch_contents(&missing).expect("missing dir is ok");
    assert_eq!(removed, 0);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn write_params_file_writes_exact_content_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "pyexec-params-test-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(PARAMS_FILE_NAME);
    write_params_file(&path, r#"{"blob":"xyz"}"#).unwrap();
    let back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(back, r#"{"blob":"xyz"}"#);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "params file must be private (0600)");
    std::fs::remove_dir_all(&dir).ok();
}
