# Operator env overlay (#458) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop `kastellan-cli install` from silently destroying operator configuration — add a `kastellan.env.local` overlay the installer never writes, name every key an install is about to drop or change, and make a disabled egress force-routing loud.

**Architecture:** `ServiceSpec` grows an ordered list of environment files instead of a single optional one; systemd renders one `EnvironmentFile=` directive per entry (later-wins is systemd's own semantics, measured on the DGX) and launchd folds them in the same order into the plist. The env-file grammar lifts into one `cfg`-free module so the installer's new diff and the launchd fold share a single parser. The installer diffs the file it is about to overwrite and backs it up when that diff is non-empty.

**Tech Stack:** Rust 2021, `serde` / `serde_json`, `tracing`, `tempfile` (dev), `sqlx` (Postgres audit insert). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-08-operator-env-overlay-design.md`

## Global Constraints

- **AGPL-compatible dependencies only.** This plan adds none.
- **Cross-platform, Linux + macOS first-class.** `supervisor/src/env_file.rs` is `cfg`-free deliberately, so its tests run on both hosts (#511 `atomic_write` precedent) — the functions it holds previously lived in the macOS-only `launchd_agents` module and never compiled on Linux at all.
- **The two backends compile on one host each** — `systemd_user` is `#[cfg(target_os = "linux")]`, `launchd_agents` is `#[cfg(target_os = "macos")]`. So a `launchd_agents.rs` change is invisible to the DGX and a `systemd_user.rs` change is invisible to the Mac. Gate any task touching a backend on **both** hosts, and expect the two hosts to report **different test counts**.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at exit 0.
- **Run cargo in the FOREGROUND. Never background a `cargo test` or `cargo clippy` and wait on it.**
- **Source the toolchain first:** every shell step assumes `source "$HOME/.cargo/env"`.
- **On the Mac, use a private target dir** — the IDE's rust-analyzer holds `target/debug/.cargo-lock`. Prefix cargo commands with `CARGO_TARGET_DIR=$HOME/.cache/kastellan-458-target`. It must live under `$HOME`, never `/tmp` (macOS scrubs `/tmp` mid-run).
- **`git add <specific files>`, never `git add -A`.** Untracked files in this tree must not be swept in.
- **Precedence rule, measured not assumed:** `EnvironmentFile=` overrides `Environment=`; a later `EnvironmentFile=` overrides an earlier one; `EnvironmentFile=-<missing>` starts normally.

---

## File Structure

**Created**
- `supervisor/src/env_file.rs` — the one env-file grammar (`parse_env_file`, `merge_env`), `cfg`-free, `pub`.
- `supervisor/src/env_file/tests.rs` — its tests, moved verbatim from the launchd builders.
- `core/src/install/env_diff.rs` — pure `diff_env_files` + `EnvDiff`.
- `core/src/install/env_diff/tests.rs` — its tests.
- `core/src/egress/force_routing_notice.rs` — the disabled-force-routing phrase const, audit action/actor consts, and pure payload builder.

**Modified**
- `supervisor/src/lib.rs` — add `pub mod env_file;`, add `EnvFileRef`, replace `ServiceSpec.environment_file` with `environment_files`.
- `supervisor/src/systemd_user/builder.rs:130-135` — render N directives.
- `supervisor/src/systemd_user.rs:384-390` — control-character validation over the list.
- `supervisor/src/launchd_agents.rs:261-279` — fold N files in order, skip absent optional.
- `supervisor/src/launchd_agents/builders.rs:233-270` — delete the two lifted fns.
- `supervisor/src/specs.rs:103,169` — literal updates.
- `core/src/install/plan.rs:20-42` (`Layout`), `:458-463` (`build_specs`).
- `core/src/install/mod.rs` — add `pub mod env_diff;`.
- `core/src/install/run.rs:59-60` — diff, backup, warn, then write.
- `core/src/egress/mod.rs` — add `pub mod force_routing_notice;`.
- `core/src/main.rs:191-192` — add the `else` arm.
- Test literal sites (15 `environment_file: None`), listed per task.

---

### Task 1: Lift the env-file grammar into one `cfg`-free module

Pure movement, no behaviour change — its own commit so the diff is reviewable on its own (the #525 tests-split precedent).

**Files:**
- Create: `supervisor/src/env_file.rs`
- Create: `supervisor/src/env_file/tests.rs`
- Modify: `supervisor/src/lib.rs` (add module declaration)
- Modify: `supervisor/src/launchd_agents/builders.rs` (delete lines 233-270)
- Modify: `supervisor/src/launchd_agents/builders/tests.rs` (delete lines 281-310)
- Modify: `supervisor/src/launchd_agents.rs` (import from the new home)

**Interfaces:**
- Consumes: nothing.
- Produces: `kastellan_supervisor::env_file::parse_env_file(&str) -> Vec<(String, String)>` and `kastellan_supervisor::env_file::merge_env(&mut Vec<(String,String)>, Vec<(String,String)>)`. Task 2 (the launchd fold) and Task 4 (the installer's diff) both depend on these exact signatures.

- [ ] **Step 1: Create the new module with the two functions moved verbatim**

Create `supervisor/src/env_file.rs`:

```rust
//! The `EnvironmentFile=` grammar, in one place.
//!
//! Deliberately `cfg`-free and shared rather than per-backend. The launchd
//! backend folds these pairs into a plist (launchd has no `EnvironmentFile=`
//! directive) and `kastellan-core`'s installer uses the same parser to diff the
//! env file it is about to overwrite. A second parser for one file format is
//! the drift shape #479 and #520 each cost a review round; and shared code is
//! compiled and tested on **both** hosts, while per-backend code is invisible
//! to CI (there is no macOS job at all) — the same reasoning that folded the
//! two backends' staging helpers into one `atomic_write` in #511.

/// Parse an `EnvironmentFile`-style buffer into ordered `(KEY, value)` pairs.
///
/// Pure (no I/O). Matches the subset of systemd's `EnvironmentFile=` grammar
/// the installer emits: one `KEY=value` per line, blank lines and `#` comments
/// skipped, surrounding whitespace on the key trimmed. Values are taken
/// verbatim after the first `=` (no shell expansion, no quote stripping) since
/// the installer writes plain values. Lines without `=` are ignored.
pub fn parse_env_file(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                out.push((k.to_string(), v.to_string()));
            }
        }
    }
    out
}

/// Merge `from` into `into`, with `from` winning on key collision (matching
/// systemd's `EnvironmentFile=`-after-`Environment=` override order, and the
/// later-file-wins order between two `EnvironmentFile=` directives — both
/// measured on a live systemd user manager, not assumed). Existing keys keep
/// their position with the value replaced; new keys are appended.
pub fn merge_env(into: &mut Vec<(String, String)>, from: Vec<(String, String)>) {
    for (k, v) in from {
        if let Some(slot) = into.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            into.push((k, v));
        }
    }
}

#[cfg(test)]
mod tests;
```

- [ ] **Step 2: Move the two tests verbatim**

Create `supervisor/src/env_file/tests.rs` with the two tests currently at
`supervisor/src/launchd_agents/builders/tests.rs:283-310`, unchanged except for the import:

```rust
use super::*;

#[test]
fn parse_env_file_skips_comments_blanks_and_keeps_embedded_equals() {
    let parsed = parse_env_file("# header\n\nFOO=bar\n  BAZ =qux=zap\nnokey\n");
    assert_eq!(
        parsed,
        vec![
            ("FOO".to_string(), "bar".to_string()),
            // key trimmed; value taken verbatim after the first '=' (so an
            // embedded '=', e.g. a URL query, is preserved). Lines without '='
            // ("nokey") and '#' comments are skipped.
            ("BAZ".to_string(), "qux=zap".to_string()),
        ]
    );
}

#[test]
fn merge_env_file_values_override_inline_env_keeping_position() {
    let mut env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
    merge_env(&mut env, vec![("B".into(), "override".into()), ("C".into(), "3".into())]);
    assert_eq!(
        env,
        vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "override".to_string()), // overridden in place
            ("C".to_string(), "3".to_string()),        // new key appended
        ]
    );
}
```

- [ ] **Step 3: Delete the originals and rewire**

In `supervisor/src/launchd_agents/builders.rs`, delete `parse_env_file` (lines 233-257) and `merge_env` (lines 259-270).
In `supervisor/src/launchd_agents/builders/tests.rs`, delete the banner comment and both tests (lines 281-310).
In `supervisor/src/lib.rs`, add beside the other module declarations (near line 19):

```rust
pub mod env_file;
```

In `supervisor/src/launchd_agents.rs`, replace the `builders::parse_env_file` / `builders::merge_env` call sites (around line 274) with `crate::env_file::parse_env_file` / `crate::env_file::merge_env`.

- [ ] **Step 4: Verify the move changed nothing**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-supervisor`
Expected: PASS, with the **same total test count as before the move** (the two tests moved, none were added or lost). Record the count — the baseline is 85 on the Mac.

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/env_file.rs supervisor/src/env_file/tests.rs supervisor/src/lib.rs \
        supervisor/src/launchd_agents.rs supervisor/src/launchd_agents/builders.rs \
        supervisor/src/launchd_agents/builders/tests.rs
git commit -m "refactor(supervisor): lift the EnvironmentFile grammar into one cfg-free module

Pure movement, no behaviour change. parse_env_file/merge_env were pub(super)
inside the launchd builders; the installer's env diff (#458) needs the same
grammar and core already depends on kastellan-supervisor. One parser, one
home, compiled and tested on both hosts."
```

---

### Task 2: `EnvFileRef` + `ServiceSpec.environment_files`, rendered by both backends

> **Merged from the plan's original Tasks 2 and 3.** A `ServiceSpec` field rename
> spans both backends, so splitting it leaves an intermediate where
> `kastellan-supervisor` does not compile — `launchd_agents.rs:268` reads the old
> field. There is no half a reviewer could meaningfully approve.

**Files:**
- Modify: `supervisor/src/lib.rs:126-136` (field), plus `EnvFileRef` definition
- Modify: `supervisor/src/systemd_user/builder.rs:130-135`
- Modify: `supervisor/src/systemd_user.rs:384-399`
- Modify: `supervisor/src/launchd_agents.rs:261-279`
- Modify: `supervisor/src/specs.rs:103`, `:169`
- Modify (literal sites, `environment_file: None` → `environment_files: Vec::new()`): `supervisor/src/lib.rs:408`, `:511`; `supervisor/src/systemd_user/tests.rs:32`; `supervisor/src/systemd_user/builder/tests.rs:23`; `supervisor/src/launchd_agents/tests.rs:28`; `supervisor/src/launchd_agents/builders/tests.rs:23`, `:183`; `supervisor/tests/target_smoke.rs:62`; `supervisor/tests/systemd_user_smoke.rs:154`, `:248`; `supervisor/tests/launchd_agents_smoke.rs:143`, `:203`, `:247`
- Test: `supervisor/src/systemd_user/builder/tests.rs`, `supervisor/src/launchd_agents/tests.rs`

**Interfaces:**
- Consumes: `env_file::{parse_env_file, merge_env}` (Task 1).
- Produces: `kastellan_supervisor::EnvFileRef { path: PathBuf, optional: bool }` and `ServiceSpec.environment_files: Vec<EnvFileRef>`. Task 3 consumes both.

- [ ] **Step 1: Write the failing tests**

Replace `environment_file_rendered_when_set` and `environment_file_absent_when_none` in `supervisor/src/systemd_user/builder/tests.rs:246-262` with:

```rust
#[test]
fn environment_files_render_in_order_with_optional_prefixed() {
    let mut spec = minimal_spec("svc");
    spec.environment_files = vec![
        EnvFileRef { path: std::path::PathBuf::from("/home/u/.config/kastellan/kastellan.env"), optional: false },
        EnvFileRef { path: std::path::PathBuf::from("/home/u/.config/kastellan/kastellan.env.local"), optional: true },
    ];
    let unit = build_unit_file(&spec);
    let lines: Vec<&str> = unit.lines().filter(|l| l.starts_with("EnvironmentFile=")).collect();
    // Order is load-bearing: systemd applies these in file order and a LATER
    // file overrides an earlier one, which is the whole mechanism by which the
    // operator's `.local` beats the regenerated `kastellan.env`.
    assert_eq!(
        lines,
        vec![
            "EnvironmentFile=/home/u/.config/kastellan/kastellan.env",
            "EnvironmentFile=-/home/u/.config/kastellan/kastellan.env.local",
        ],
        "{unit}"
    );
}

#[test]
fn environment_files_absent_when_empty() {
    let spec = minimal_spec("svc");
    let unit = build_unit_file(&spec);
    assert!(!unit.contains("EnvironmentFile="), "{unit}");
}
```

Add `use kastellan_supervisor::EnvFileRef;` (or `use crate::EnvFileRef;` — match the file's existing import style) at the top of that test file.


Then, in `supervisor/src/launchd_agents/tests.rs`, replace `install_folds_environment_file_into_plist_env_vars` and `install_errors_when_environment_file_missing` (lines 198-228) with:


Replace `install_folds_environment_file_into_plist_env_vars` and `install_errors_when_environment_file_missing` in `supervisor/src/launchd_agents/tests.rs:198-228` with:

```rust
#[test]
fn install_folds_environment_files_in_order_with_later_winning() {
    let dir = TestRoot::new("env-file");
    let base = dir.path().join("kastellan.env");
    let local = dir.path().join("kastellan.env.local");
    fs::write(&base, "# tuned by operator\nKASTELLAN_DATA_DIR=/srv/data\nKASTELLAN_LLM_LOCAL_MODEL=stock-tag\n").unwrap();
    // The overlay overrides the regenerated value and adds a key of its own —
    // the two shapes #458 exists to fix.
    fs::write(&local, "KASTELLAN_LLM_LOCAL_MODEL=tuned-tag\nKASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443\n").unwrap();

    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.environment_files = vec![
        EnvFileRef { path: base, optional: false },
        EnvFileRef { path: local, optional: true },
    ];
    sup.install(&spec).expect("install");

    let body = fs::read_to_string(sup.plist_path("kastellan-core")).unwrap();
    assert!(body.contains("<key>KASTELLAN_DATA_DIR</key>"), "{body}");
    assert!(body.contains("<string>/srv/data</string>"), "{body}");
    assert!(body.contains("<string>tuned-tag</string>"), "{body}");
    assert!(!body.contains("<string>stock-tag</string>"), "later file must win: {body}");
    assert!(body.contains("<key>KASTELLAN_MAIL_ENDPOINT</key>"), "{body}");
}

#[test]
fn install_skips_a_missing_optional_environment_file() {
    let dir = TestRoot::new("env-file-optional-missing");
    let base = dir.path().join("kastellan.env");
    fs::write(&base, "KASTELLAN_DATA_DIR=/srv/data\n").unwrap();
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    // The normal case: the operator has not created a `.local` yet.
    spec.environment_files = vec![
        EnvFileRef { path: base, optional: false },
        EnvFileRef { path: dir.path().join("kastellan.env.local"), optional: true },
    ];
    sup.install(&spec).expect("a missing OPTIONAL env file must not fail the install");
    let body = fs::read_to_string(sup.plist_path("svc")).unwrap();
    assert!(body.contains("<key>KASTELLAN_DATA_DIR</key>"), "{body}");
}

#[test]
fn install_errors_when_a_required_environment_file_is_missing() {
    let dir = TestRoot::new("env-file-missing");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files = vec![EnvFileRef {
        path: dir.path().join("does-not-exist.env"),
        optional: false,
    }];
    let err = sup.install(&spec).expect_err("missing REQUIRED env file");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}
```

Add `use kastellan_supervisor::EnvFileRef;` (matching the file's import style) if not already in scope.

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-supervisor`
Expected: FAIL to compile — `no field 'environment_files' on type 'ServiceSpec'`.

- [ ] **Step 3: Add the type and change the field**

In `supervisor/src/lib.rs`, add above `ServiceSpec`:

```rust
/// One `EnvironmentFile=` entry on a [`ServiceSpec`].
///
/// Entries are applied **in order, later winning on key collision** — systemd's
/// own semantics for repeated `EnvironmentFile=` directives, measured on a live
/// user manager rather than recalled. That ordering is the mechanism by which an
/// operator's `kastellan.env.local` overrides the `kastellan.env` the installer
/// regenerates on every deploy (#458).
///
/// `optional` is a real field rather than a convention because the two backends
/// need it for different reasons: systemd needs the `-` prefix, and the launchd
/// backend reads the file at install time and would otherwise hard-error on a
/// `.local` that does not exist — which is the normal case.
///
/// **Platform asymmetry, stated rather than hidden.** systemd re-reads these at
/// every service start, so an edit takes effect on `restart`. launchd has no
/// `EnvironmentFile=` directive, so the backend folds the pairs into the plist
/// at *install* time and an edit needs a re-install. The guarantee #458 asks for
/// — surviving a reinstall — holds identically on both; only the refresh moment
/// differs, and it is inherent to launchd.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnvFileRef {
    pub path: PathBuf,
    /// A missing file is not an error: renders systemd's `-` prefix, and the
    /// launchd backend skips it instead of failing the install.
    #[serde(default)]
    pub optional: bool,
}
```

Replace the `environment_file` field (lines 126-136) with:

```rust
    /// Ordered `EnvironmentFile=` entries (KEY=value lines) the service reads on
    /// start. Empty (the default) renders no directive — byte-identical to a
    /// spec that set nothing. The installer points the core daemon at
    /// `~/.config/kastellan/kastellan.env` (required) followed by
    /// `~/.config/kastellan/kastellan.env.local` (optional, never written by the
    /// installer), so operator tuning survives a reinstall. See [`EnvFileRef`]
    /// for the ordering and platform notes.
    #[serde(default)]
    pub environment_files: Vec<EnvFileRef>,
```

- [ ] **Step 4: Update every literal site and the two spec builders**

Change all 13 `environment_file: None,` occurrences listed in **Files** above to `environment_files: Vec::new(),`. In `supervisor/src/systemd_user.rs`, replace the fixed-size validation array (lines 384-390) with:

```rust
        let mut path_fields: Vec<(&str, &PathBuf)> = vec![("program", &spec.program)];
        for ef in &spec.environment_files {
            path_fields.push(("environment_file", &ef.path));
        }
        for (field, p) in spec
            .working_dir
            .as_ref()
            .map(|p| ("working_dir", p))
            .into_iter()
            .chain(spec.stdout_log.as_ref().map(|p| ("stdout_log", p)))
            .chain(spec.stderr_log.as_ref().map(|p| ("stderr_log", p)))
        {
            path_fields.push((field, p));
        }
        for (field, p) in path_fields {
            if p.to_string_lossy().contains(|c: char| c.is_control()) {
                return Err(SupervisorError::Io(format!(
                    "{field} must not contain control characters, got {p:?}"
                )));
            }
        }
```

- [ ] **Step 5: Render the list in the systemd builder**

Replace `supervisor/src/systemd_user/builder.rs:130-135` with:

```rust
    // One directive per entry, in order. systemd applies them in file order with
    // a LATER file overriding an earlier one; the `-` prefix makes a missing file
    // non-fatal. Both behaviours were measured on a live user manager (#458).
    for ef in &spec.environment_files {
        let prefix = if ef.optional { "-" } else { "" };
        out.push_str(&format!(
            "EnvironmentFile={prefix}{}\n",
            quote_if_needed(&ef.path.to_string_lossy())
        ));
    }
```

- [ ] **Step 6: Implement the fold**

Replace `supervisor/src/launchd_agents.rs:267-279` with:

```rust
        let path = self.plist_path(&spec.name);
        // launchd has no `EnvironmentFile=` directive, so fold each file's pairs
        // into the plist's `EnvironmentVariables` at install time — in declared
        // order, later winning, matching what systemd does at start time. An
        // absent OPTIONAL file is skipped (that is the normal state of
        // `kastellan.env.local`); an absent REQUIRED one is still an error.
        let mut merged = spec.clone();
        for ef in &spec.environment_files {
            let contents = match fs::read_to_string(&ef.path) {
                Ok(c) => c,
                Err(e) if ef.optional && e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(SupervisorError::Io(format!(
                        "read environment_file {}: {e}",
                        ef.path.display()
                    )))
                }
            };
            crate::env_file::merge_env(&mut merged.env, crate::env_file::parse_env_file(&contents));
        }
        merged.environment_files = Vec::new(); // already folded into env
        let body = build_plist(&merged);
        write_atomic(&path, body.as_bytes())?;
```

Update the doc comment above it (lines 261-266) to describe the list rather than the single file.

- [ ] **Step 7: Run tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-supervisor`
Expected on the **Mac**: PASS, **+1 test** over Task 1 (the systemd builder tests are replaced 2-for-2; the two launchd tests become three). Expected on the **DGX**: **+0** — `launchd_agents` is `#[cfg(target_os = "macos")]`, so the launchd tests do not exist there and the systemd replacement is net zero. A differing count between hosts is correct here, not a skipped test.

- [ ] **Step 8: Clippy, and cross-lint the Linux arm from the Mac**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-supervisor --all-targets -- -D warnings`
Run (Mac only): `source "$HOME/.cargo/env" && cargo clippy -p kastellan-supervisor --target aarch64-unknown-linux-gnu -- -D warnings`
Expected: exit 0 both. The crate is pure-Rust so the cross-lint type-checks the systemd arm without a linker.

- [ ] **Step 9: Commit**

```bash
git add supervisor/src/lib.rs supervisor/src/systemd_user.rs supervisor/src/systemd_user/builder.rs \
        supervisor/src/systemd_user/builder/tests.rs supervisor/src/systemd_user/tests.rs \
        supervisor/src/specs.rs supervisor/src/launchd_agents.rs supervisor/src/launchd_agents/tests.rs \
        supervisor/src/launchd_agents/builders/tests.rs supervisor/tests/target_smoke.rs \
        supervisor/tests/systemd_user_smoke.rs supervisor/tests/launchd_agents_smoke.rs
git commit -m "feat(supervisor): ordered EnvironmentFile list with per-entry optionality

ServiceSpec.environment_file (Option<PathBuf>) becomes environment_files
(Vec<EnvFileRef>), applied in order with later winning -- systemd's own
semantics, measured on a live user manager. systemd renders one directive per
entry, '-'-prefixed when optional; launchd folds them in the same order and
skips a missing OPTIONAL file (the normal state of kastellan.env.local) while
still erroring on a missing REQUIRED one. Groundwork for #458's overlay."
```

---

### Task 3: The installer points the core service at both files

**Files:**
- Modify: `core/src/install/plan.rs:20-42` (`Layout`), `:458-463` (`build_specs`)
- Test: `core/src/install/plan.rs` tests module (near `:738`)

**Interfaces:**
- Consumes: `EnvFileRef`, `ServiceSpec.environment_files` (Task 2).
- Produces: `Layout.env_local_file: PathBuf`. Task 5 does **not** need it (the diff is on `env_file`), but Task 7 (docs) does.

- [ ] **Step 1: Write the failing test**

Replace `specs_point_core_at_installed_binary_and_env_file`'s env assertion (`core/src/install/plan.rs:744`) and add:

```rust
    #[test]
    fn specs_point_core_at_the_generated_env_then_the_operator_overlay() {
        let l = layout();
        let specs = build_specs(&l, Path::new("/usr/lib/postgresql/16/bin/postgres"));
        let core = specs.members.iter().find(|s| s.name == "kastellan-core").unwrap();
        // Order is the mechanism: systemd applies these in order and the LATER
        // file wins, so the operator's `.local` overrides anything `install`
        // regenerates. Reversing them would silently restore #458.
        assert_eq!(core.environment_files.len(), 2);
        assert_eq!(core.environment_files[0].path, l.env_file);
        assert!(!core.environment_files[0].optional);
        assert_eq!(core.environment_files[1].path, l.env_local_file);
        assert!(
            core.environment_files[1].optional,
            "the overlay must be optional — the installer never creates it"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan::tests::specs_point_core`
Expected: FAIL to compile — no field `env_local_file`.

- [ ] **Step 3: Implement**

In `core/src/install/plan.rs`, add to `Layout` beside `env_file` (line 20):

```rust
    /// The operator overlay. **Never written by the installer** — it exists so
    /// hand-tuned settings survive the `kastellan.env` regeneration that
    /// otherwise drops them on every deploy (#458). Listed after `env_file` on
    /// the service spec, so its values win.
    pub env_local_file: PathBuf,
```

and in the constructor beside line 42:

```rust
        env_local_file: config_dir.join("kastellan.env.local"),
```

Replace `build_specs`'s line 461 with:

```rust
    core.environment_files = vec![
        kastellan_supervisor::EnvFileRef { path: layout.env_file.clone(), optional: false },
        kastellan_supervisor::EnvFileRef { path: layout.env_local_file.clone(), optional: true },
    ];
```

Update the `Layout` assertion test at line 631 to also assert `env_local_file`.

- [ ] **Step 4: Run tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::`
Expected: PASS, +1 test.

- [ ] **Step 5: Commit**

```bash
git add core/src/install/plan.rs
git commit -m "feat(install): point the core service at kastellan.env.local

Generated env first, operator overlay second and optional. systemd's
later-wins ordering makes the overlay beat anything install regenerates,
which is what stops a deploy silently dropping tuned config (#458)."
```

---

### Task 4: `diff_env_files` — the pure diff

**Files:**
- Create: `core/src/install/env_diff.rs`
- Create: `core/src/install/env_diff/tests.rs`
- Modify: `core/src/install/mod.rs`

**Interfaces:**
- Consumes: `kastellan_supervisor::env_file::parse_env_file` (Task 1).
- Produces: `EnvDiff { lost: Vec<String>, changed: Vec<String>, is_empty() -> bool }` and `diff_env_files(old: &str, new: &str) -> EnvDiff`. Task 5 consumes both.

- [ ] **Step 1: Write the failing tests**

Create `core/src/install/env_diff/tests.rs`:

```rust
use super::*;

#[test]
fn a_key_present_in_old_and_absent_in_new_is_lost() {
    let d = diff_env_files("KASTELLAN_MAIL_ENDPOINT=https://h:8443\nKASTELLAN_DATA_DIR=/d\n",
                           "KASTELLAN_DATA_DIR=/d\n");
    assert_eq!(d.lost, vec!["KASTELLAN_MAIL_ENDPOINT"]);
    assert!(d.changed.is_empty());
    assert!(!d.is_empty());
}

#[test]
fn a_key_whose_value_differs_is_changed() {
    // The exact live shape: install reverts a hand-tuned model tag to the
    // flag default, and nothing says so.
    let d = diff_env_files("KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0-ctx64k\n",
                           "KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0\n");
    assert!(d.lost.is_empty());
    assert_eq!(d.changed, vec!["KASTELLAN_LLM_LOCAL_MODEL"]);
}

#[test]
fn an_identical_file_diffs_to_nothing() {
    let s = "KASTELLAN_DATA_DIR=/d\nKASTELLAN_LLM_LOCAL_URL=http://h/v1\n";
    let d = diff_env_files(s, s);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_key_only_in_new_is_not_reported() {
    // The installer adding a key is it doing its job, not a loss.
    let d = diff_env_files("A=1\n", "A=1\nB=2\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn commented_and_malformed_lines_are_ignored_on_both_sides() {
    // `render_env_file` emits commented defaults (# KASTELLAN_TIMEZONE=...);
    // a comment is not a key, on either side.
    let d = diff_env_files("# KASTELLAN_TIMEZONE=Australia/Sydney\nnokey\n\nA=1\n",
                           "# KASTELLAN_TIMEZONE=Australia/Sydney\nA=1\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_key_the_operator_uncommented_is_reported_as_lost() {
    // Old file has it live; the freshly rendered file has it commented out.
    // The value really is being lost, so `lost` is the honest answer.
    let d = diff_env_files("KASTELLAN_TIMEZONE=Australia/Sydney\n",
                           "# KASTELLAN_TIMEZONE=Australia/Sydney\n");
    assert_eq!(d.lost, vec!["KASTELLAN_TIMEZONE"]);
}

#[test]
fn a_repeated_key_is_compared_on_its_last_value_not_its_first() {
    // systemd takes the last occurrence, and env_file::merge_env already
    // behaves that way. Comparing the first would report a change that did
    // not happen.
    let d = diff_env_files("A=old\nA=real\n", "A=real\n");
    assert!(d.is_empty(), "nothing changed; operative old value is `real`: {d:?}");
}

#[test]
fn a_revert_of_a_repeated_keys_last_value_is_reported() {
    // The dangerous direction: comparing first values makes this look
    // unchanged, so a real revert goes silently unreported -- the exact #458
    // failure this feature exists to prevent.
    let d = diff_env_files("A=1\nA=2\n", "A=1\n");
    assert_eq!(d.changed, vec!["A"]);
    assert!(d.lost.is_empty());
}

#[test]
fn reported_keys_follow_the_old_files_order_and_do_not_repeat() {
    let d = diff_env_files("Z=1\nA=2\nZ=3\n", "");
    // Deterministic for a stable operator-facing message; a duplicated key in
    // the source file is reported once.
    assert_eq!(d.lost, vec!["Z", "A"]);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::env_diff`
Expected: FAIL to compile — module does not exist.

- [ ] **Step 3: Implement**

Create `core/src/install/env_diff.rs`:

```rust
//! What an install is about to destroy, named before it destroys it.
//!
//! `kastellan-cli install` regenerates `kastellan.env` from CLI flags, so any
//! hand-added key is dropped and any hand-tuned value reverts to the flag
//! default. On 2026-08-08 that silently removed the deployed agent's mail
//! capability for two days: with `KASTELLAN_MAIL_ENDPOINT` gone the `mail.*`
//! tools never registered, the planner fell back to filesystem probing, and the
//! only symptom was a wrong answer. See [#458].
//!
//! This module is the pure half of the fix: compare the file about to be
//! overwritten against the freshly rendered content and report the difference by
//! **key name only**. Values stay out of the install transcript — the operator
//! reads them from the `.bak` copy the caller writes — because an env file may
//! one day hold something that should not be echoed to a terminal.
//!
//! [#458]: https://github.com/hherb/kastellan/issues/458

use kastellan_supervisor::env_file::parse_env_file;

/// Keys an install would drop or change, in the old file's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvDiff {
    /// Present in the old file, absent from the new one.
    pub lost: Vec<String>,
    /// Present in both with a different value.
    pub changed: Vec<String>,
}

impl EnvDiff {
    /// True when the install destroys nothing — the common case, and the
    /// condition under which the caller writes no backup and prints nothing.
    pub fn is_empty(&self) -> bool {
        self.lost.is_empty() && self.changed.is_empty()
    }
}

/// Diff two `EnvironmentFile` buffers by key.
///
/// Only uncommented `KEY=value` lines count, via the shared
/// [`kastellan_supervisor::env_file::parse_env_file`] grammar — so the commented
/// defaults `render_env_file` emits are not mistaken for keys, and a key the
/// operator *uncommented* is correctly reported as lost.
///
/// Keys present only in `new` are not reported: that is the installer adding
/// something, not destroying it. Output follows `old`'s line order so the
/// operator-facing message is deterministic, and each key is reported at most
/// once even if the source file repeats it.
pub fn diff_env_files(old: &str, new: &str) -> EnvDiff {
    // Both sides are bound to locals first: `parse_env_file` returns an owned
    // Vec, and iterating it inline would drop the temporary while the borrowed
    // &str keys are still in use.
    let old_pairs = parse_env_file(old);
    let new_pairs = parse_env_file(new);

    // A repeated key's OPERATIVE value is its LAST occurrence -- systemd's own
    // behaviour, and already what `env_file::merge_env` does when it folds pairs
    // in order. Comparing the first occurrence instead would both invent changes
    // that did not happen and, worse, silently miss real ones.
    let last = |pairs: &[(String, String)], key: &str| -> Option<String> {
        pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };

    let mut diff = EnvDiff::default();
    let mut seen: Vec<&str> = Vec::new();
    // Report order follows the old file by FIRST appearance, so the
    // operator-facing message is stable, even though the compared value comes
    // from the last.
    for key in old_pairs.iter().map(|(k, _)| k.as_str()) {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let old_value = last(&old_pairs, key).expect("key came from old_pairs");
        match last(&new_pairs, key) {
            None => diff.lost.push(key.to_string()),
            Some(new_value) if new_value != old_value => diff.changed.push(key.to_string()),
            Some(_) => {}
        }
    }
    diff
}

#[cfg(test)]
mod tests;
```

Add to `core/src/install/mod.rs`:

```rust
pub mod env_diff;
```

- [ ] **Step 4: Run tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::env_diff`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add core/src/install/env_diff.rs core/src/install/env_diff/tests.rs core/src/install/mod.rs
git commit -m "feat(install): pure diff of the env file an install is about to overwrite

Reports lost keys and changed values by NAME only, on the shared env-file
grammar so commented defaults are not mistaken for keys. Groundwork for the
install-time warning (#458)."
```

---

### Task 5: `install` backs the file up and names what it is dropping

**Files:**
- Modify: `core/src/install/run.rs:59-60`
- Test: `core/src/install/run/tests.rs` (already exists; `run.rs:512` declares `#[cfg(test)] mod tests;` — append to it, do not create a new module)

**Interfaces:**
- Consumes: `env_diff::{diff_env_files, EnvDiff}` (Task 4), `Layout.env_file` (existing).
- Produces: `pub(crate) fn preserve_and_report_env(env_file: &Path, new_contents: &str) -> Result<(), String>` — writes the backup and prints the warning; called immediately before the env write.

- [ ] **Step 1: Write the failing test**

In the run tests module:

```rust
use super::*;
use std::fs;

#[test]
fn a_destructive_rewrite_backs_the_old_file_up() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    fs::write(&env, "KASTELLAN_MAIL_ENDPOINT=https://h:8443\n").unwrap();

    preserve_and_report_env(&env, "KASTELLAN_DATA_DIR=/d\n").unwrap();

    let bak = dir.path().join("kastellan.env.bak");
    assert_eq!(
        fs::read_to_string(&bak).unwrap(),
        "KASTELLAN_MAIL_ENDPOINT=https://h:8443\n",
        "the backup is where the operator reads the values the warning omits"
    );
}

#[test]
fn a_non_destructive_rewrite_writes_no_backup() {
    // Gating on a non-empty diff is what stops a later clean install from
    // clobbering the one backup that mattered.
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    fs::write(&env, "A=1\n").unwrap();

    preserve_and_report_env(&env, "A=1\nB=2\n").unwrap();

    assert!(!dir.path().join("kastellan.env.bak").exists());
}

#[test]
fn a_first_install_has_nothing_to_preserve() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    preserve_and_report_env(&env, "A=1\n").expect("a missing env file is not an error");
    assert!(!dir.path().join("kastellan.env.bak").exists());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::run`
Expected: FAIL to compile — `preserve_and_report_env` not found.

- [ ] **Step 3: Implement**

Add to `core/src/install/run.rs`:

```rust
/// Back up and report on the `kastellan.env` an install is about to overwrite.
///
/// `install` regenerates the env file from CLI flags, so every hand-added key is
/// dropped and every hand-tuned value reverts (#458). Silence there cost the
/// deployed agent its mail capability for two days. This makes the loss loud and
/// recoverable:
///
/// * nothing to lose (fresh install, or a purely additive rewrite) ⇒ no backup,
///   no output — the common case stays quiet;
/// * otherwise ⇒ copy the current file to `kastellan.env.bak` and name every key
///   being dropped or changed, pointing at `kastellan.env.local` as the fix.
///
/// **Key names only, never values.** The operator reads values from the backup;
/// keeping them out of the install transcript means an env file that one day
/// holds a secret does not echo it to a terminal.
pub(crate) fn preserve_and_report_env(env_file: &Path, new_contents: &str) -> Result<(), String> {
    let Ok(old) = fs::read_to_string(env_file) else {
        return Ok(()); // first install: nothing to preserve
    };
    let diff = crate::install::env_diff::diff_env_files(&old, new_contents);
    if diff.is_empty() {
        return Ok(());
    }

    let bak = env_file.with_extension("env.bak");
    write_private(&bak, old.as_bytes())?;

    eprintln!("warning: install is regenerating {}", env_file.display());
    for k in &diff.lost {
        eprintln!("  dropped: {k}");
    }
    for k in &diff.changed {
        eprintln!("  changed: {k}");
    }
    eprintln!(
        "  previous file saved to {}\n  \
         to keep these across future installs, move them into {} —\n  \
         the installer never writes that file, and its values override this one.",
        bak.display(),
        env_file.with_extension("env.local").display()
    );
    Ok(())
}
```

Then change lines 59-60 to:

```rust
    let env = render_env_file(args, layout);
    preserve_and_report_env(&layout.env_file, &env)?;
    write_private(&layout.env_file, env.as_bytes())?;
```

Note on `with_extension`: `kastellan.env` has extension `env`, so `with_extension("env.bak")` yields `kastellan.env.bak` and `with_extension("env.local")` yields `kastellan.env.local`. **Verify this in the test** rather than trusting it — `with_extension` *replaces* the final component, which is exactly the trap #511 found in `write_atomic`. If it misbehaves, build the paths from `layout.env_file.file_name()` instead.

- [ ] **Step 4: Run tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::`
Expected: PASS, +3 tests.

- [ ] **Step 5: Clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add core/src/install/run.rs core/src/install/run/tests.rs
git commit -m "feat(install): back up and name what a regenerated env file destroys

Silence here cost the deployed agent its mail capability for two days (#458).
A destructive rewrite now copies the file to kastellan.env.bak and names every
dropped/changed key, pointing at kastellan.env.local. Key names only -- values
stay out of the install transcript."
```

---

### Task 6: A disabled force-routing becomes loud

**Files:**
- Create: `core/src/egress/force_routing_notice.rs`
- Create: `core/src/egress/force_routing_notice/tests.rs`
- Modify: `core/src/egress/mod.rs`
- Modify: `core/src/main.rs:191-192`

**Interfaces:**
- Consumes: nothing.
- Produces: `FORCE_ROUTING_DISABLED_LOG_PHRASE`, `ACTOR`, `ACTION_FORCE_ROUTING_DISABLED`, `force_routing_disabled_payload()`.

- [ ] **Step 1: Write the failing tests**

Create `core/src/egress/force_routing_notice/tests.rs`:

```rust
use super::*;

#[test]
fn the_action_and_actor_are_stable_audit_contract() {
    // Renaming either breaks operator queries against audit_log.
    assert_eq!(ACTION_FORCE_ROUTING_DISABLED, "egress.force_routing_disabled");
    assert_eq!(ACTOR, "daemon");
}

#[test]
fn the_payload_names_the_env_var_that_controls_it() {
    let p = force_routing_disabled_payload();
    assert_eq!(p["env_var"], "KASTELLAN_EGRESS_FORCE_ROUTING");
    // The operator-visible phrase travels WITH the row, so an audit reader and
    // a log grepper are looking at the same string.
    assert_eq!(p["phrase"], FORCE_ROUTING_DISABLED_LOG_PHRASE);
}

#[test]
fn the_log_phrase_is_a_const_not_a_literal() {
    // #516/#524/#525 all shipped an operator-facing phrase as a bare literal
    // beside a const that existed for exactly that purpose. Assert through the
    // const so any rename moves both sides at once.
    assert!(FORCE_ROUTING_DISABLED_LOG_PHRASE.contains("FORCE-ROUTING DISABLED"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::force_routing_notice`
Expected: FAIL to compile — module does not exist.

- [ ] **Step 3: Implement**

Create `core/src/egress/force_routing_notice.rs`:

```rust
//! The daemon's startup notice when egress force-routing is OFF.
//!
//! `main.rs` logged an `info!` when force-routing was ON and said **nothing at
//! all** when it was off — the `if let Some(..)` had no `else`. With it off,
//! host workers fall back to `--share-net` with only the in-worker allowlist,
//! and no line, row or metric records that.
//!
//! That silence matters more than it looks. The unit sets
//! `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1`, but systemd applies
//! `EnvironmentFile=` **after** `Environment=` (measured on a live user manager,
//! not assumed), so the env file the installer regenerates — and the operator
//! overlay beside it — can turn this off. A posture that an ordinary config file
//! can flip must announce itself.
//!
//! The actor is `daemon`, not `egress_proxy`: this is the daemon's own startup
//! posture, and attributing it to a proxy that by definition is not running
//! would be wrong.

/// Operator-facing phrase, grep-able in `~/.local/state/kastellan/*.out`.
/// A `const` on purpose: three separate changes (#516, #524, #525) shipped an
/// operator-facing phrase as a literal typed twice and drifted.
pub const FORCE_ROUTING_DISABLED_LOG_PHRASE: &str = "EGRESS FORCE-ROUTING DISABLED";

/// Audit actor for daemon-level startup posture rows.
pub const ACTOR: &str = "daemon";

/// Audit action. Renaming is an audit-trail contract break.
pub const ACTION_FORCE_ROUTING_DISABLED: &str = "egress.force_routing_disabled";

/// Payload for the `egress.force_routing_disabled` row.
///
/// Pure, so the wire shape is unit-testable without a live pool.
pub fn force_routing_disabled_payload() -> serde_json::Value {
    serde_json::json!({
        "phrase": FORCE_ROUTING_DISABLED_LOG_PHRASE,
        "env_var": "KASTELLAN_EGRESS_FORCE_ROUTING",
        "consequence": "Net::Allowlist workers spawn with a direct network route; \
                        only the in-worker allowlist applies, and no egress proxy \
                        enforces host:port or SSRF checks.",
    })
}

#[cfg(test)]
mod tests;
```

Add to `core/src/egress/mod.rs` (alphabetical, after `cert_pins`):

```rust
pub mod force_routing_notice;
```

- [ ] **Step 4: Wire it into `main.rs`**

Replace `core/src/main.rs:191-192`'s `if let` with:

```rust
    if let Some(fr) = force_routing.as_ref() {
        info!("egress force-routing ENABLED — Net::Allowlist workers route through the egress proxy");
```

...leaving the existing body, and add an `else` arm at its close:

```rust
    } else {
        use kastellan_core::egress::force_routing_notice as frn;
        warn!(
            env_var = "KASTELLAN_EGRESS_FORCE_ROUTING",
            "{} — Net::Allowlist workers get a direct network route; no egress \
             proxy enforces host:port or SSRF. Set it to 1 in kastellan.env.local \
             unless this is a deliberate bring-up without the proxy.",
            frn::FORCE_ROUTING_DISABLED_LOG_PHRASE
        );
        // Best-effort: the posture belongs in the oversight record, not only in
        // a plaintext log with no role gating. A failed insert must not stop a
        // daemon that is otherwise healthy.
        if let Err(e) = kastellan_db::audit::insert(
            &pool,
            frn::ACTOR,
            frn::ACTION_FORCE_ROUTING_DISABLED,
            frn::force_routing_disabled_payload(),
        )
        .await
        {
            warn!(error = %e, "could not audit the disabled force-routing posture");
        }
    }
```

Ensure `warn` is imported in `main.rs` (it uses `tracing::info` already — add `warn` to the same `use`).

- [ ] **Step 5: Run tests + build**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::force_routing_notice`
Expected: PASS, 3 tests.
Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bins`
Expected: success.

- [ ] **Step 6: Clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add core/src/egress/force_routing_notice.rs core/src/egress/force_routing_notice/tests.rs \
        core/src/egress/mod.rs core/src/main.rs
git commit -m "feat(egress): announce a disabled force-routing instead of saying nothing

main.rs logged ENABLED and had no else arm at all, so the weaker posture was
silent -- and since EnvironmentFile= overrides Environment=, an ordinary config
file can flip it. Now a warn! plus an egress.force_routing_disabled audit row,
with the operator phrase as a const (#458)."
```

---

### Task 7: Operator documentation

**Files:**
- Modify: `core/src/install/plan.rs` — `render_env_file` gains a header comment naming the overlay
- Modify: `scripts/upgrade_from_git.sh` — comment near the install step
- Create: `docs/deploy/operator-env.md` (the directory already exists and holds `matrix-homeserver.md`)

**Interfaces:**
- Consumes: `Layout.env_local_file` (Task 3).
- Produces: nothing.

- [ ] **Step 1: Write the failing test**

In `core/src/install/plan.rs` tests:

```rust
    #[test]
    fn the_generated_env_file_tells_operators_about_the_overlay() {
        // The generated file is the first place an operator looks, and it is
        // the file that gets destroyed. It must name its own successor.
        let s = render_env_file(&test_args("m", "http://h:1", None), &layout());
        assert!(s.contains("kastellan.env.local"), "{s}");
        assert!(
            s.contains("regenerated"),
            "it must say this file is regenerated, not merely that another exists: {s}"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan::tests::the_generated_env_file`
Expected: FAIL — the string is absent.

- [ ] **Step 3: Implement**

At the top of `render_env_file` (before the first `push_str`), add:

```rust
    s.push_str(
        "# GENERATED BY `kastellan-cli install` — DO NOT HAND-EDIT.\n\
         # This file is regenerated from CLI flags on every install, so any key\n\
         # you add here is dropped and any value you tune here reverts.\n\
         # Put operator settings in kastellan.env.local instead: the installer\n\
         # never writes that file, and its values override the ones below.\n\n",
    );
```

- [ ] **Step 4: Write the operator doc**

Create `docs/deploy/operator-env.md`:

```markdown
# Operator environment settings

`kastellan-cli install` **regenerates** `~/.config/kastellan/kastellan.env` from
its CLI flags every time it runs. Any key you add to that file is dropped on the
next deploy, and any value you tune there reverts to the flag default.

Put operator settings in `~/.config/kastellan/kastellan.env.local` instead.

- The installer **never writes** that file.
- Its values **win**: both backends apply environment files in order, later
  overriding earlier, and the overlay is listed second.
- It is optional — if it does not exist, nothing changes.
- Give it `0600` permissions, like the file it sits beside.

## Example

```sh
cat > ~/.config/kastellan/kastellan.env.local <<'EOF'
KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0-ctx64k
KASTELLAN_LLM_TIMEOUT_MS=180000
KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443
KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
EOF
chmod 600 ~/.config/kastellan/kastellan.env.local
```

## When it takes effect

| platform | mechanism | an edit takes effect |
| --- | --- | --- |
| Linux (systemd) | a second `EnvironmentFile=` directive | next `systemctl --user restart kastellan-core` |
| macOS (launchd) | folded into the plist at install time | next `kastellan-cli install` |

launchd has no `EnvironmentFile=` directive, so the values are baked in when the
plist is written. Surviving a reinstall works identically on both platforms; only
the refresh moment differs.

## If an install reports dropped keys

```
warning: install is regenerating /home/you/.config/kastellan/kastellan.env
  dropped: KASTELLAN_MAIL_ENDPOINT
  changed: KASTELLAN_LLM_LOCAL_MODEL
  previous file saved to /home/you/.config/kastellan/kastellan.env.bak
```

Those keys were still in the generated file. Copy them out of the `.bak` into
`kastellan.env.local` and restart. The backup is only written when something is
actually being lost, so it is not overwritten by a later clean install.
```

- [ ] **Step 5: Document the overlay in the upgrade script**

In `scripts/upgrade_from_git.sh`, above the `install` step (near line 81), add:

```sh
# NOTE: `install` REGENERATES $ENV_FILE from CLI flags. Operator settings belong
# in ${ENV_FILE}.local, which the installer never writes and whose values win
# (systemd applies EnvironmentFile= directives in order, later winning). If the
# install reports dropped or changed keys, they were still in the generated file
# — move them into the .local and re-run. See issue #458.
```

- [ ] **Step 6: Run tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::`
Expected: PASS, +1 test.

- [ ] **Step 7: Commit**

```bash
git add core/src/install/plan.rs scripts/upgrade_from_git.sh docs/deploy/operator-env.md
git commit -m "docs(install): the generated env file names its own successor

An operator's first stop is the file that gets destroyed, so it now says so
and points at kastellan.env.local (#458)."
```

---

## Final verification (not a task — run before opening the PR)

- [ ] **Mac, targeted** (private target dir — rust-analyzer holds the default lock):

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cache/kastellan-458-target
cargo test -p kastellan-supervisor
cargo test -p kastellan-core --lib install::
cargo test -p kastellan-core --lib egress::force_routing_notice
cargo clippy -p kastellan-supervisor --all-targets -- -D warnings
cargo clippy -p kastellan-core --all-targets -- -D warnings
cargo clippy -p kastellan-supervisor --target aarch64-unknown-linux-gnu -- -D warnings
```

- [ ] **DGX, full workspace** — write the log to `$HOME`, never `/tmp` (scrubbed mid-run on both hosts):

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  (cargo test --workspace -- --nocapture > ~/gate-458.log 2>&1; echo TEST_EXIT=$? >> ~/gate-458.log; \
   cargo clippy --workspace --all-targets -- -D warnings >> ~/gate-458.log 2>&1; \
   echo CLIPPY_EXIT=$? >> ~/gate-458.log; echo DONE-SENTINEL >> ~/gate-458.log)'
```

Expected: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, and the passed count equal to **3047 + the tests this plan adds on Linux**. Task 2's +1 is entirely in the macOS-only launchd tests, so it contributes **+0 on the DGX**: Task 2 +0, Task 3 +1, Task 4 +9, Task 5 +3, Task 6 +3, Task 7 +1 = **+17 ⇒ 3064** on the DGX; the Mac sees +18) and confirm the run lands on that number. A count that misses the prediction means a test was silently skipped or a file is not compiled on that host — investigate before shipping. Confirm exactly 4 `[SKIP]` lines, all the `KASTELLAN_GLINER_RELEX_ENABLE` tier.

- [ ] **Live acceptance on the DGX — today's failure, re-run as a test.** This is the point of the change; do not skip it.

```sh
# 1. Move the five tuned settings into the overlay.
ssh dgx 'ENV=~/.config/kastellan/kastellan.env; \
  grep -E "^KASTELLAN_(EGRESS_UPSTREAM_EXTRA_CA|LLM_TIMEOUT_MS|MAIL_ENDPOINT|MAIL_TOKEN_FILE|LLM_LOCAL_MODEL)=" "$ENV" > "$ENV.local"; \
  chmod 600 "$ENV.local"; cat "$ENV.local" | cut -c1-60'
# 2. Reinstall (the operation that used to destroy them).
ssh dgx 'cd ~/src/kastellan && bash scripts/upgrade_from_git.sh > ~/upgrade-458.log 2>&1; \
  echo EXIT=$? >> ~/upgrade-458.log; grep -E "dropped:|changed:|saved to" ~/upgrade-458.log'
# 3. The proof: the PROCESS carries them (the file being right is not the same).
ssh dgx 'PID=$(systemctl --user show kastellan-core -p MainPID --value); \
  tr "\0" "\n" < /proc/$PID/environ | grep -cE "^KASTELLAN_(EGRESS_UPSTREAM_EXTRA_CA|LLM_TIMEOUT_MS|MAIL_ENDPOINT|MAIL_TOKEN_FILE)="; \
  tr "\0" "\n" < /proc/$PID/environ | grep -E "^KASTELLAN_(LLM_LOCAL_MODEL|EGRESS_FORCE_ROUTING)="'
```

Expected: the grep count is **4**, the model tag still ends in `-ctx64k`, `KASTELLAN_EGRESS_FORCE_ROUTING=1`, and no `EGRESS FORCE-ROUTING DISABLED` line in the daemon log.

- [ ] **Live end-to-end:** `ssh dgx '~/.local/bin/kastellan-cli ask "summarize my most recent email"'` still answers from the real archive. That is the capability #458 removed.
