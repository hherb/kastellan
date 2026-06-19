# Design — `kastellan-cli install` / `uninstall` (per-user supervised install)

**Date:** 2026-06-19
**Status:** approved (brainstorm), pre-implementation
**Context:** There is no installer today — the `supervisor` specs (`postgres`/`core`/`kastellan.target`) and `kastellan-db-init` exist but nothing wires them together into a running, supervised deployment (the deferred "Slice 3 operator surface"). An operator who wants a persistent Kastellan (not throwaway test scaffolds) has no supported path. This adds `kastellan-cli install`/`uninstall`. Resolves HANDOVER open-question #6 (production install-location convention).

**Guiding principles (in priority order):** least friction · predictability · "likely to just work". Concretely: idempotent (safe to re-run), fail-closed with actionable error messages (every failure says what to do next), sensible defaults (one required flag), and a real post-install verification step that confirms the service is actually up.

## Goal

One command takes a freshly-built tree (`cargo build --release`) to a running, supervised, per-user Kastellan: Postgres + the agent daemon under `systemd --user`, with binaries and assets copied to a stable per-user prefix so the running service does not depend on the git checkout.

Non-goals: building from source (operator runs `cargo build --release` first); venv-based workers (browser-driver, gliner-relex — opt-in, installed by their own `scripts/workers/*`); system-wide/root installs; full macOS acceptance (macOS works via `--pg-bin-dir` but is verified in a later round).

## Per-user layout (the install convention)

```
~/.local/lib/kastellan/                  # ALL workspace binaries, FLAT (daemon finds workers
                                          # via current_exe()-relative discovery — the intended "flat install")
    kastellan  kastellan-cli  kastellan-db-init
    kastellan-worker-egress-proxy  -shell-exec  -web-fetch  -web-search
    kastellan-worker-python-exec  -matrix  -lockdown-exec
~/.local/share/kastellan/
    prompts/                              # copied from repo prompts/
    seeds/memory/l0_meta_rules.toml       # copied from repo seeds/
    pg/data/                              # the Postgres cluster (db-init)
~/.config/kastellan/kastellan.env         # EnvironmentFile (operator-tunable)
~/.config/systemd/user/{kastellan-postgres.service,kastellan-core.service,kastellan.target}
~/.local/state/kastellan/                 # log dir (StandardOutput/Error append targets), pre-created
```

Rationale (per-user): Kastellan is personal — own daemon, own Postgres cluster, own OS-keyring secrets. Only the LLM is shared/served externally. So a per-user prefix (no root) is the right unit of installation.

## Command surface

```
kastellan-cli install [--llm-model <name>] [--llm-url <url>]
                      [--pg-bin-dir <dir>] [--from <built-bin-dir>] [--no-start]
kastellan-cli uninstall [--purge]
```

- `--llm-model <name>` — **required** (the daemon's router needs a model; no safe default). Empty/missing → fail with a message naming the flag.
- `--llm-url <url>` — defaults per-OS: Linux `http://127.0.0.1:8000`, macOS `http://127.0.0.1:11434`.
- `--from <dir>` — directory holding the freshly-built binaries; defaults to the directory of the running `kastellan-cli` (`current_exe` parent — i.e. `target/release/` when run from a build tree).
- `--pg-bin-dir <dir>` — override Postgres binary discovery (the daemon/db-init default candidates exclude macOS Postgres.app, so macOS requires this).
- `--no-start` — install + db-init, but do not `enable`/`start` the target (operator starts manually).
- `uninstall` — stop the target, remove the units, daemon-reload. `--purge` *additionally* deletes the prefix + `~/.local/share/kastellan` data dir (cluster + secrets!) — destructive, requires typed `purge` confirmation. Default keeps all data.

## What `install` does (sequence)

1. **Resolve + validate.** Compute the `Layout` from `$HOME`/`$USER` (fail with a clear message if unset). Resolve `--llm-model` (required) and `--llm-url` (default).
2. **Discover built binaries** in `--from`. The required set is the daemon + the Cargo-binary workers (see layout). **Fail closed** if any is missing, naming the missing binary and suggesting `cargo build --release`. (venv workers are not in this set.)
3. **Create dirs + copy.** Create the prefix, assets, log, and config dirs (idempotent). Copy each binary to the prefix and `prompts/`, `seeds/` to the assets dir. Copy via temp-path + atomic rename so a re-run can't leave a half-written binary. Re-running overwrites cleanly.
4. **db-init (idempotent).** Invoke the just-copied `kastellan-db-init` binary (from the prefix) with `--data-dir <prefix>/pg/data` and, if `--pg-bin-dir` was given, `--bin-dir <dir>`. That binary is already idempotent (skips if `PG_VERSION` present) and is the single source of truth for cluster init — the installer does not reimplement initdb. A non-zero exit fails the install with the db-init output and guidance (e.g. PG not found → install PostgreSQL 18 / pass `--pg-bin-dir`).
5. **Write the EnvironmentFile** `~/.config/kastellan/kastellan.env`, mode 0600, containing: `KASTELLAN_LLM_LOCAL_URL`, `KASTELLAN_LLM_LOCAL_MODEL`, `KASTELLAN_PROMPTS_DIR` (→ prefix assets), `KASTELLAN_L0_RULES_FILE` (→ assets seed), `KASTELLAN_DATA_DIR` (→ `pg/data`). Rendered by a pure function.
6. **Build specs + install_target.** Construct `postgres_service_spec`/`core_service_spec`/`kastellan_target_spec` with absolute prefix paths (postgres binary, the installed `kastellan`, log dir) and the EnvironmentFile on the core spec; call `Supervisor::install_target` (writes units + one daemon-reload).
7. **Linger + start.** On Linux, `loginctl enable-linger $USER` (so `--user` services survive logout on a headless box; no-op/skipped on macOS). Then `systemctl --user enable --now kastellan.target` — unless `--no-start`.
8. **Verify (the "just works" gate).** Poll for the PG socket (`<data>/sockets/.s.PGSQL.5432`) up to a timeout, then check `systemctl --user is-active kastellan-postgres kastellan-core` — both must be `active`. (No DB query needed; service-active + socket-present is the predictable signal.) On success print the install summary + status command; on failure print the exact `journalctl --user -u kastellan-core -n 50` (and `-u kastellan-postgres`) commands and exit non-zero, leaving the units installed for inspection.

## Supervisor change

Add `environment_file: Option<PathBuf>` to `ServiceSpec`. `build_unit_file` renders `EnvironmentFile=<path>` after the existing `Environment=` lines (a missing/None field renders nothing — byte-identical to today for all current callers). `core_service_spec` gains a parameter to set it (and keeps `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1`). Covered by a `build_unit_file` unit test (EnvironmentFile present vs absent).

## Structure + testing

**Pure plan module — `core/src/install/plan.rs`** (no I/O; unit-tested):
- `resolve_layout(home: &Path, user: &str) -> Layout` — all the paths above.
- `render_env_file(model, url, layout) -> String` — the kastellan.env contents.
- `required_binaries() -> &'static [&'static str]` — the daemon + Cargo-worker names to copy.
- `build_specs(layout, pg_bin_path, log_dir) -> InstallSpecs` — the three `ServiceSpec`s/`TargetSpec` with absolute paths + EnvironmentFile.
- `default_llm_url() -> &'static str` (per-OS) and arg parsing → `InstallArgs`.

**IO orchestration — `core/src/bin/kastellan-cli/install.rs`** (thin; calls the plan): binary discovery, dir creation, copy (temp+rename), db-init, env-file write, `install_target`, linger, `systemctl`, verify, plus `uninstall`. Each external failure is mapped to an actionable message.

**Tests:**
- Unit (no I/O): `resolve_layout` (XDG paths off a fake `$HOME`), `render_env_file` (every required key present, correct prefix paths), `required_binaries` non-empty + includes egress-proxy + matrix + lockdown-exec, `build_specs` (core spec references the installed `kastellan` path + the EnvironmentFile; target Wants both services), arg parsing (missing `--llm-model` errors; `--llm-url` default; flag parsing). Plus the supervisor `build_unit_file` EnvironmentFile test.
- Integration (`core/tests/install_e2e.rs`, gated; skip-as-pass where it can't run): drive the **file-producing half** against a temp `$HOME`/XDG with `--no-start` and PG/systemctl steps stubbed or skipped — assert the prefix is populated with the binaries, `kastellan.env` is rendered with the right keys/paths, and the three unit files are written referencing the prefix paths + EnvironmentFile. **Live acceptance = running `kastellan-cli install` on the DGX** (real `systemd --user` + PG + daemon start + a `secret list` afterward); that is the real gate, since the live path needs systemd + PG + an LLM endpoint.

## Verification

`cargo test -p kastellan-core` (unit + gated integration), `cargo clippy --workspace --all-targets -D warnings`. **DGX acceptance:** `cargo build --release` → `kastellan-cli install --llm-model <served-model>` → `systemctl --user status kastellan.target` green, `kastellan-cli secret list` connects (no error), then the operator stores `matrix_kastellan_password`.

## Cross-platform

Cross-platform via the `Supervisor` trait (`SystemdUser`/`LaunchAgents`) + `--pg-bin-dir` (macOS/Postgres.app). Linger is Linux-only (skipped on macOS). Verified on the DGX (Linux) this round; macOS runs via the override, full macOS acceptance is a follow-up. No platform-only code without a trait-provided counterpart (honors the cross-platform constraint).
