//! Hermetic test of the file-producing half of `kastellan-cli install`
//! (`kastellan_core::install::prepare_filesystem`). No systemd, no PG —
//! drives the copy + env-file generation against a temp HOME and a fake
//! build dir. The live install (db-init + systemd start) is verified by
//! running `kastellan-cli install` on a real host (the DGX).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::Path;

use kastellan_core::install::plan::{resolve_layout, required_binaries, InstallArgs};
use kastellan_core::install::run::prepare_filesystem;

fn touch_exec(dir: &Path, name: &str) {
    let p = dir.join(name);
    fs::write(&p, b"#!/bin/sh\ntrue\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn prepare_filesystem_populates_prefix_and_env_file() {
    let tmp = std::env::temp_dir().join(format!("kastellan-install-test-{}", std::process::id()));
    let home = tmp.join("home");
    let from = tmp.join("from");
    let assets_src = tmp.join("src");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(assets_src.join("prompts")).unwrap();
    fs::create_dir_all(assets_src.join("seeds/memory")).unwrap();
    fs::write(assets_src.join("prompts/system.txt"), b"hi").unwrap();
    fs::write(assets_src.join("seeds/memory/l0_meta_rules.toml"), b"[x]\n").unwrap();
    for b in required_binaries() {
        touch_exec(&from, b);
    }

    let layout = resolve_layout(&home, "tester");
    let args = InstallArgs {
        llm_model: "test-model".into(),
        llm_url: "http://127.0.0.1:8000".into(),
        embedding_model: None,
        pg_bin_dir: None,
        from: Some(from.clone()),
        no_start: true,
    };

    let copied = prepare_filesystem(&layout, &from, &assets_src, &args).expect("prepare_filesystem");

    // Required binaries landed in the flat prefix, executable.
    for b in required_binaries() {
        let dest = layout.bin_dir.join(b);
        assert!(dest.is_file(), "missing installed binary {b}");
        assert!(copied.contains(&b.to_string()));
    }
    // Assets copied.
    assert!(layout.prompts_dir.join("system.txt").is_file());
    assert!(layout.l0_rules_file.is_file());
    // Env file rendered with the model + prefix data dir.
    let env = fs::read_to_string(&layout.env_file).unwrap();
    assert!(env.contains("KASTELLAN_LLM_LOCAL_MODEL=test-model\n"));
    assert!(env.contains(&format!("KASTELLAN_DATA_DIR={}\n", layout.data_dir.display())));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&layout.env_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "kastellan.env must be mode 0600");
    }

    fs::remove_dir_all(&tmp).ok();
}

#[test]
fn prepare_filesystem_fails_closed_on_missing_required_binary() {
    let tmp = std::env::temp_dir().join(format!("kastellan-install-miss-{}", std::process::id()));
    let home = tmp.join("home");
    let from = tmp.join("from");
    let assets_src = tmp.join("src");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(assets_src.join("prompts")).unwrap();
    fs::create_dir_all(assets_src.join("seeds/memory")).unwrap();
    // Deliberately copy only ONE required binary → must fail.
    touch_exec(&from, "kastellan");

    let layout = resolve_layout(&home, "tester");
    let args = InstallArgs {
        llm_model: "m".into(), llm_url: "u".into(), embedding_model: None,
        pg_bin_dir: None, from: Some(from.clone()), no_start: true,
    };
    let err = prepare_filesystem(&layout, &from, &assets_src, &args).unwrap_err();
    assert!(
        err.contains("kastellan-cli")
            || err.contains("kastellan-db-init")
            || err.contains("kastellan-worker-egress-proxy"),
        "error should name a missing required binary; got: {err}"
    );

    fs::remove_dir_all(&tmp).ok();
}
