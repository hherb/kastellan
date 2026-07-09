# web-research Firecracker micro-VM entry — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the `web-research` composite worker an opt-in Firecracker micro-VM execution mode (`KASTELLAN_WEB_RESEARCH_USE_MICROVM=1`, Linux only), mirroring the existing web-fetch VM entry, forwarding the SearxNG + embed endpoint config.

**Architecture:** Add a Linux-gated `web_research_firecracker_entry` + a `resolve` short-circuit inside the single `WebResearchManifest` (no new registry line — same runtime-branch pattern web-fetch uses). Factor the shared env assembly into a pure `base_env` helper so the embed-env logic lives in one place. Add a rootfs build script and a DGX-gated e2e, both cloned from web-fetch's.

**Tech Stack:** Rust (kastellan-core, kastellan-sandbox), Firecracker micro-VM backend, bash rootfs script, `#[cfg(target_os = "linux")]` gating.

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible deps only.** No new dependency is added by this plan.
- **Cross-platform Linux + macOS.** All VM code is `#[cfg(target_os = "linux")]`; macOS never reads the gate nor references the `FirecrackerVm` variant (issue-#144 rule). Host path is byte-identical when the gate is unset.
- **rustc 1.96.0.** Dev Mac (Seatbelt) + DGX Spark (aarch64, native Linux, real KVM over `ssh dgx '<cmd>'`).
- **Files under 500 LOC where feasible.** `core/src/workers/web_research.rs` is 279 LOC today; re-`wc -l` at the end — if the additions cross 500, lift the `#[cfg(test)] mod tests` to a sibling `web_research/tests.rs` (established pattern), do NOT prod-split for this feature.
- **TDD, frequent commits.** Write the failing test, watch it fail, implement minimally, watch it pass, commit.
- **Subagent hygiene (from memory notes):** run all `cargo` commands in the **FOREGROUND** (no background waits). `git add <specific files>` only — never `git add -A` (untracked scratch files must stay out).
- **Source cargo env first** in every shell: `source "$HOME/.cargo/env"`.
- **The worker is sandboxed before it runs.** No unsandboxed escape hatch. Trust boundary unchanged: URLs come from SearxNG, every fetch is `hit_allowed` + egress-gated; VM mode additionally force-routes egress through the MITM sidecar.

---

## File Structure

- **Modify** `core/src/workers/web_research.rs` — add `base_env` helper, refactor `web_research_entry_with_embed` to use it, add Linux consts + `web_research_firecracker_entry` + the `resolve` VM branch + 2 Linux-gated unit tests. (Tasks 1–2)
- **Create** `scripts/workers/microvm/build-web-research-rootfs.sh` — clone of `build-web-fetch-rootfs.sh` for web-research. (Task 3)
- **Create** `core/tests/web_research_firecracker_egress_e2e.rs` — DGX-gated e2e, clone of `web_fetch_firecracker_egress_e2e.rs`. (Task 4)
- **Modify** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — record completion. (Task 6)

---

## Task 1: VM entry function + shared `base_env` helper

**Files:**
- Modify: `core/src/workers/web_research.rs` (add helper + refactor host entry + add VM entry + Linux consts + Linux unit test)

**Interfaces:**
- Consumes: `crate::workers::web_fetch::allowlist_to_net_entries` (existing, pub(crate)); `kastellan_sandbox::{Net, Profile, SandboxPolicy, SandboxBackendKind}`.
- Produces:
  - `fn base_env(endpoint: &str, embed_endpoint: Option<&str>, embed_model: Option<&str>, allowlist: &[String]) -> Vec<(String, String)>` (private) — the shared env vec: `[ENDPOINT_ENV, "KASTELLAN_WEB_RESEARCH_ALLOWLIST", (EMBED_ENDPOINT_ENV, EMBED_MODEL_ENV)?]`.
  - `#[cfg(target_os = "linux")] pub fn web_research_firecracker_entry(binary: PathBuf, image_dir: String, endpoint: &str, embed_endpoint: Option<&str>, embed_model: Option<&str>, allowlist: &[String]) -> ToolEntry`.
  - Consts `#[cfg(target_os="linux")] const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_MICROVM";` and `const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-research";`.

- [ ] **Step 1: Write the failing test** (append inside the existing `#[cfg(test)] mod tests` block in `core/src/workers/web_research.rs`, before the closing `}` and the `outcome_label` helper). This mirrors web-fetch's `firecracker_entry_is_net_allowlist_vm_with_empty_fs_read`, extended to assert the embed env is forwarded:

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_vm_with_empty_fs_read_and_forwarded_env() {
        let allowlist = vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let entry = web_research_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
            "/var/lib/kastellan/microvm".to_string(),
            "https://searx.example.org:8888/search",
            Some("http://embed.example.org:11434/v1/embeddings"),
            None, // default model
            &allowlist,
        );
        // VM backend, net client, no host paths shared in (the CA is added at spawn).
        assert!(matches!(
            entry.sandbox_backend,
            Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
        ));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty (no NIC, no local DNS)");
        assert!(entry.policy.proxy_uds.is_none(), "proxy_uds is set at spawn, not in the manifest");
        assert_eq!(entry.policy.cpu_ms, 15_000);
        assert_eq!(entry.policy.mem_mb, 512);
        assert_eq!(entry.wall_clock_ms, Some(60_000));
        // Union Net::Allowlist: endpoint host:port, embed host:port, content host:443.
        match &entry.policy.net {
            Net::Allowlist(hosts) => {
                assert_eq!(hosts[0], "searx.example.org:8888", "endpoint host:port first");
                assert!(hosts.iter().any(|h| h == "embed.example.org:11434"), "embed host:port present: {hosts:?}");
                assert!(hosts.iter().any(|h| h == "docs.example.org:443"), "content host:443 present: {hosts:?}");
            }
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // Env forwards endpoint + verbatim allowlist + embed endpoint/model + the VM image dir + rootfs.
        let env = &entry.policy.env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get(ENDPOINT_ENV), Some("https://searx.example.org:8888/search"));
        assert_eq!(get("KASTELLAN_WEB_RESEARCH_ALLOWLIST"), Some(r#"["searx.example.org",".docs.example.org"]"#));
        assert_eq!(get(EMBED_ENDPOINT_ENV), Some("http://embed.example.org:11434/v1/embeddings"));
        assert_eq!(get(EMBED_MODEL_ENV), Some("embeddinggemma"), "default model forwarded");
        assert_eq!(get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(get("KASTELLAN_MICROVM_ROOTFS"), Some("web-research.ext4"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research::tests::firecracker_entry_is_vm_with_empty_fs_read_and_forwarded_env`
Expected: FAIL to compile — `cannot find function web_research_firecracker_entry in this scope`.

- [ ] **Step 3: Add the Linux consts.** After the existing const block (`DEFAULT_EMBED_MODEL`, line ~25), add:

```rust
/// Opt into the Linux Firecracker micro-VM backend for web-research. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_MICROVM";

/// In-rootfs path of the web-research worker binary (staged there by
/// `build-web-research-rootfs.sh`). Used by the micro-VM entry, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-research";
```

- [ ] **Step 4: Extract the `base_env` helper.** Add this pure helper directly above `web_research_entry` (line ~74):

```rust
/// The env pairs shared by the host and micro-VM entries: the SearxNG endpoint,
/// the verbatim content allowlist JSON, and — when `embed_endpoint` is set — the
/// embed endpoint + model (model defaults to [`DEFAULT_EMBED_MODEL`]). Order is
/// stable (endpoint, allowlist, [embed endpoint, embed model]); the micro-VM
/// entry appends its `KASTELLAN_MICROVM_*` pairs after these. Pure.
fn base_env(
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> Vec<(String, String)> {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let mut env = vec![
        (ENDPOINT_ENV.to_string(), endpoint.to_string()),
        ("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json),
    ];
    if let Some(embed) = embed_endpoint {
        env.push((EMBED_ENDPOINT_ENV.to_string(), embed.to_string()));
        env.push((
            EMBED_MODEL_ENV.to_string(),
            embed_model.unwrap_or(DEFAULT_EMBED_MODEL).to_string(),
        ));
    }
    env
}
```

- [ ] **Step 5: Refactor `web_research_entry_with_embed` to use `base_env`.** Replace the `let allow_json = …; let mut env = vec![…]; if let Some(embed) = … { … }` block (lines 91–102) with a single call. The function body becomes:

```rust
pub fn web_research_entry_with_embed(
    binary: PathBuf,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let env = base_env(endpoint, embed_endpoint, embed_model, allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}
```

- [ ] **Step 6: Add `web_research_firecracker_entry`.** Add directly after `web_research_entry_with_embed` (with the loopback-embed caveat in the doc-comment, spec change #5):

```rust
/// Build the [`ToolEntry`] for web-research running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1`, Linux only). Mirrors the
/// host-mode [`web_research_entry_with_embed`] but as a VM net worker:
///
/// * `Net::Allowlist` = the same union (SearxNG endpoint ∪ embed ∪ content) as
///   host mode — **not** `Net::Deny`; web-research needs egress. Force-routing sets
///   `proxy_uds` at spawn, which makes `build_launch_plan` boot the VM with no NIC
///   and tunnel egress over the slice-4a vsock channel.
/// * `fs_read: vec![]` — no NIC and no local DNS (the egress proxy resolves
///   host-side). The per-instance MITM CA is appended to `fs_read` at spawn by
///   `rewrite_worker_policy`.
/// * `env` forwards the host env ([`base_env`]) plus `KASTELLAN_MICROVM_DIR` and
///   `KASTELLAN_MICROVM_ROOTFS=web-research.ext4` so the backend boots the right rootfs.
///
/// **Loopback-embed caveat:** in VM mode all egress tunnels through the host-side
/// proxy, which SSRF-blocks loopback/private IPs. So the *default* embed endpoint
/// (local Ollama `127.0.0.1:11434`) is unreachable → the query embed fails and the
/// worker degrades to lexical ranking with an `embed_note` (never silent). For
/// hybrid ranking in VM mode, point `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` at a
/// **routable** embed host. Host mode is unaffected.
///
/// Linux-only: emits the `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let mut env = base_env(endpoint, embed_endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}
```

- [ ] **Step 7: Run the new test + the existing host tests to verify all pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research`
Expected: PASS — the new `firecracker_entry_is_vm_with_empty_fs_read_and_forwarded_env` plus the 3 existing tests (`resolve_registers_union_net_and_injects_env`, `resolve_unions_embed_endpoint_into_net_and_injects_env`, `resolve_misconfigured_when_no_binary_found`) all green (the refactor is behaviour-preserving; the existing tests are the guard).

- [ ] **Step 8: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: clean (exit 0).

- [ ] **Step 9: Commit**

```bash
git add core/src/workers/web_research.rs
git commit -m "feat(web-research): web_research_firecracker_entry + shared base_env helper

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Manifest `resolve` VM short-circuit

**Files:**
- Modify: `core/src/workers/web_research.rs` (`WebResearchManifest::resolve` + a Linux unit test)

**Interfaces:**
- Consumes: `web_research_firecracker_entry` (Task 1), `USE_MICROVM_ENV`, `MICROVM_WORKER_BIN` (Task 1); the existing `ResolveCtx` fields `get_env`, `allowlist`.
- Produces: no new public symbol — `resolve` gains a Linux branch returning the VM entry when opted in.

- [ ] **Step 1: Write the failing test** (append inside `#[cfg(test)] mod tests`, mirroring web-fetch's `resolve_uses_microvm_entry_when_opted_in`):

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_when_opted_in() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // In-rootfs binary path, not a host-discovered binary.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-web-research")
                );
                let env = &entry.policy.env;
                let dir = env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR").map(|(_, v)| v.as_str());
                assert_eq!(dir, Some("/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research::tests::resolve_uses_microvm_entry_when_opted_in`
Expected: FAIL — `panicked … expected Register(VM entry)` (the current `resolve` returns the host entry with `sandbox_backend: None`, so the `matches!` assert fails). NOTE: it must reach `Register`, not `Misconfigured` — `exists` returns true so host `discover_binary` would still succeed; the assertion that fails is the `FirecrackerVm` backend match.

- [ ] **Step 3: Add the VM short-circuit to `resolve`.** In `WebResearchManifest::resolve`, immediately after the `binary` discovery block is the current flow; instead insert the branch **before** the endpoint/allowlist reads and the final `Register`. Restructure `resolve` so the Linux branch short-circuits before host binary discovery (mirrors web-fetch). Replace the body of `resolve` with:

```rust
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        let embed_endpoint = (ctx.get_env)(EMBED_ENDPOINT_ENV).filter(|s| !s.trim().is_empty());
        let embed_model = (ctx.get_env)(EMBED_MODEL_ENV).filter(|s| !s.trim().is_empty());
        let allowlist = (ctx.allowlist)(TOOL_NAME);

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                return Resolution::Register(web_research_firecracker_entry(
                    binary,
                    image_dir,
                    &endpoint,
                    embed_endpoint.as_deref(),
                    embed_model.as_deref(),
                    &allowlist,
                ));
            }
        }

        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a \
                         runnable file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        Resolution::Register(web_research_entry_with_embed(
            binary,
            &endpoint,
            embed_endpoint.as_deref(),
            embed_model.as_deref(),
            &allowlist,
        ))
    }
```

Note: `endpoint`/`embed_endpoint`/`embed_model`/`allowlist` move above the (Linux) branch so both paths read them once. On macOS the branch compiles out; the four bindings are still used by the host path below, so there is no `unused_variables` warning.

- [ ] **Step 4: Run the full web_research test module to verify all pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research`
Expected: PASS — the new `resolve_uses_microvm_entry_when_opted_in` plus all prior tests (host `resolve_*` unchanged: the gate is unset in those, so they take the host path).

- [ ] **Step 5: Clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/web_research.rs
git commit -m "feat(web-research): KASTELLAN_WEB_RESEARCH_USE_MICROVM resolve branch

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `build-web-research-rootfs.sh`

**Files:**
- Create: `scripts/workers/microvm/build-web-research-rootfs.sh`

**Interfaces:**
- Consumes: nothing at compile time. Produces `web-research.ext4` in the shared image dir, with `/usr/local/bin/kastellan-worker-web-research` matching `MICROVM_WORKER_BIN` (Task 1).

- [ ] **Step 1: Create the script** — cloned from `build-web-fetch-rootfs.sh`, changed for web-research (package name, in-rootfs binary path, ext4 label + filename):

```bash
#!/usr/bin/env bash
# Build the web-research micro-VM rootfs (ext4) into the SHARED image dir, beside
# python-exec.ext4 + web-fetch.ext4 + the shared vmlinux. The dir + kernel are
# shared across workers (build-rootfs.sh provisions them); only the rootfs
# filename differs (KASTELLAN_MICROVM_ROOTFS=web-research.ext4). web-research is a
# pure-Rust net worker: no python, and NO system CA bundle — egress (SearxNG
# search, page fetches, optional embed POSTs) is MITM-only and the only trusted
# root is the per-instance proxy CA delivered per-spawn via the slice-3 RO-share.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-web-research-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=256

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir (set the same KASTELLAN_MICROVM_DIR in the service env):" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-web-research-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if build-rootfs.sh already fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-web-research -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# Binaries: init is PID1 at /sbin/init; the worker at its in-rootfs path
# (matches MICROVM_WORKER_BIN in core/src/workers/web_research.rs).
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-web-research "$WORK/usr/local/bin/kastellan-worker-web-research"

# Shared-library closure for both Rust binaries (dynamic loader + ldd .so's),
# copied at their real absolute paths.
copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure \
    target/release/kastellan-microvm-init \
    target/release/kastellan-worker-web-research

# Pseudo-fs mountpoints (microvm-init mounts proc/sys/tmp at boot) + slice-3
# host-dir-share anchors + slice-4a /run egress relay tmpfs mountpoint. Keep this
# anchor list in lockstep with mounts.rs::SHARE_ANCHORS (opt/data/srv/mnt/work/tmp)
# and build-rootfs.sh. The per-instance ca.pem binds under /tmp (a boot tmpfs).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L web-research -d "$WORK" "$OUT_DIR/web-research.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/web-research.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: Make it executable + bash-lint it**

Run: `chmod +x scripts/workers/microvm/build-web-research-rootfs.sh && bash -n scripts/workers/microvm/build-web-research-rootfs.sh`
Expected: exit 0 (syntax OK). Do NOT run the script on the Mac — it needs `mkfs.ext4`/`ldd` (Linux). It runs on the DGX in Task 6.

- [ ] **Step 3: Commit**

```bash
git add scripts/workers/microvm/build-web-research-rootfs.sh
git commit -m "feat(web-research): build-web-research-rootfs.sh (micro-VM rootfs)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: DGX-gated Firecracker egress e2e

**Files:**
- Create: `core/tests/web_research_firecracker_egress_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::workers::web_research::web_research_firecracker_entry` (Task 1); `kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec}`; `kastellan_core::secrets::Vault`; the `kastellan_tests_common` skip helpers.
- Produces: the DGX acceptance test `web_research_vm_reaches_proxy_with_ca_delivered`.

- [ ] **Step 1: Create the test file** — cloned from `web_fetch_firecracker_egress_e2e.rs`, adapted: rootfs `web-research.ext4`, the `web_research_firecracker_entry` 6-arg call with a fixed SearxNG endpoint, and the `web.research` dispatch asserting `CONNECT searx.example.org:8888`:

```rust
#![cfg(target_os = "linux")]
//! web-research micro-VM e2e: web-research runs inside a Firecracker VM and reaches
//! the host egress proxy over the slice-4a vsock channel.
//!
//! `web_research_vm_reaches_proxy_with_ca_delivered` (hermetic, no real network;
//! still #[ignore] DGX-only — needs real KVM + vsock + the rootfs): a host
//! UnixListener stub stands in for the egress proxy at the worker's proxy_uds; a
//! force-routed web-research VM boots and one `web.research` is driven through it;
//! we assert the stub RECEIVES the worker's `CONNECT <searxng-host>:<port>` line.
//! The worker's first egress is the SearxNG search, and it can only emit CONNECT
//! after loading the in-guest CA (make_get fails closed on an unreadable
//! KASTELLAN_EGRESS_PROXY_CA), so this single assertion proves VM boot +
//! force-routing + the vsock relay + CA delivery. Embed is not exercised (mirrors
//! the web-fetch single-CONNECT gate).
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + the web-research rootfs
//! (REBUILD via build-web-research-rootfs.sh) + the kastellan-microvm-run RELEASE
//! launcher. Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     bash scripts/workers/microvm/build-web-research-rootfs.sh
//!     cargo test -p kastellan-core --test web_research_firecracker_egress_e2e -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::web_research::web_research_firecracker_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix,
};

/// SearxNG endpoint the VM worker searches first. The host part must appear in the
/// worker's CONNECT (host:port), so we pin a non-443 port to make the assertion sharp.
const SEARXNG_ENDPOINT: &str = "https://searx.example.org:8888/search";

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join("web-research.ext4") }
}

fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed (need web-research.ext4 + KVM + vsock): {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!("\n[SKIP] kastellan-microvm-run not built; run `cargo build --release -p kastellan-microvm-run`\n");
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-firecracker-egress-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Mint a self-signed CA PEM the in-VM worker will trust as KASTELLAN_EGRESS_PROXY_CA.
/// The worker's make_get fails closed on an unreadable/invalid CA, so a parseable
/// cert is required for it to build ProxyConnectGet and emit CONNECT at all.
fn write_test_ca(path: &std::path::Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let key_pair = KeyPair::generate().expect("keypair");
    let mut params = CertificateParams::new(vec!["egress-proxy.test".to_string()]).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key_pair).expect("self-signed");
    std::fs::write(path, cert.pem()).expect("write ca.pem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs"]
async fn web_research_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm() {
        return;
    }
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wr-d",
        "wr-l",
        &format!("kastellan-supervisor-test-pg-webresearch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-wr-vm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back,
    // then send a fast 503 so the worker's request fails fast instead of blocking.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line.clone());
            }
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
        }
    });

    // Force-routed web-research VM entry: set proxy_uds + the CA env + CA in fs_read,
    // exactly as rewrite_worker_policy does on the production path. No embed endpoint.
    let mut entry = web_research_firecracker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
        image_dir(),
        SEARXNG_ENDPOINT,
        None,
        None,
        &["example.com".to_string()],
    );
    entry.policy.proxy_uds = Some(uds_path.clone());
    entry.policy.env.push((
        "KASTELLAN_EGRESS_PROXY_CA".into(),
        ca_path.to_string_lossy().into_owned(),
    ));
    entry.policy.fs_read.push(ca_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-research in micro-VM");

    // Drive one web.research on a background task; we only need it to make the worker
    // attempt egress (the SearxNG search). The assertion is the stub receiving CONNECT.
    let research = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-research",
            "web.research",
            serde_json::json!({ "query": "hello world", "max_sources": 1 }),
        )
        .await;
        (worker, pool)
    });

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("stub proxy never received the in-VM worker's CONNECT (transport or CA broken)");
    assert!(
        got.starts_with("CONNECT searx.example.org:8888"),
        "expected CONNECT searx.example.org:8888 (the SearxNG search), got {got:?}"
    );

    let (worker, pool) = research.await.expect("research task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Verify it compiles for Linux.** On the Mac this file is `#![cfg(target_os = "linux")]` → it compiles to an empty crate, so `cargo build` won't type-check the body. The real check is on the DGX (Task 6). For a Mac-side pre-check that the *paths/names* used exist, confirm the imports resolve by grepping:

Run: `grep -rn "pub fn web_research_firecracker_entry" core/src/workers/web_research.rs && grep -rn "pub use.*web_research\|pub mod web_research" core/src/workers.rs core/src/workers/mod.rs 2>/dev/null; grep -rn "web_research" core/src/lib.rs 2>/dev/null`
Expected: the fn exists (Task 1) and `web_research` is a public module path (`kastellan_core::workers::web_research`) — mirrors how the web-fetch e2e imports `kastellan_core::workers::web_fetch::web_fetch_firecracker_entry`. If the module is not `pub`, check how web_fetch is exposed and match it.

- [ ] **Step 3: Commit**

```bash
git add core/tests/web_research_firecracker_egress_e2e.rs
git commit -m "test(web-research): DGX-gated Firecracker egress e2e (CONNECT-to-proxy gate)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Mac verification

**Files:** none (verification only).

- [ ] **Step 1: Workspace build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: exit 0. (`target/debug/kastellan-worker-web-research` present.)

- [ ] **Step 2: Workspace clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean (exit 0). May take several minutes — run in the FOREGROUND.

- [ ] **Step 3: aarch64 cross-clippy of the Linux-gated entry (best-effort).** This is the only Mac-side compile of the `#[cfg(target_os="linux")]` VM entry + resolve branch.

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --target aarch64-unknown-linux-gnu --lib -- -D warnings`
Expected: EITHER clean, OR a **linker/`ring` C-dep failure** (the #144 cross wall). If it fails at the *linker/cc* stage (not a type/borrow error in web_research.rs), that is the known cross wall — the Linux compile is deferred to the DGX in Task 6. If it surfaces a real type/name error in the new Linux code, fix it. Record which outcome occurred.

- [ ] **Step 4: web_research unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research`
Expected: PASS (all host tests; the Linux-gated `firecracker_*`/`resolve_uses_microvm_*` tests only run when the host is Linux — on the Mac they compile out, which is expected).

- [ ] **Step 5: Re-census the file size**

Run: `wc -l core/src/workers/web_research.rs`
Expected: report the number. If **> 500**, lift `#[cfg(test)] mod tests` into a sibling `core/src/workers/web_research/tests.rs` (add `#[path = "web_research/tests.rs"] #[cfg(test)] mod tests;` or the established module form) in a follow-up commit; if ≤ 500, no action.

- [ ] **Step 6: No commit** (verification only). Proceed to Task 6.

---

## Task 6: DGX gate discharge + handover update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

DGX access is `ssh dgx '<command>'` (the allow rule is a prefix match — flags before the hostname get denied; put everything after the hostname). Write run logs to `~` not `/tmp` (the DGX scrubs /tmp mid-run). The DGX is aarch64 native Linux with real KVM + live PG.

- [ ] **Step 1: Push the branch so the DGX can fetch it**

```bash
git push -u origin feat/web-research-microvm-entry
```

- [ ] **Step 2: On the DGX — fetch the branch, build launcher + rootfs, run the e2e.** Use a detached, log-to-home pattern (memory: DGX /tmp scrub + non-interactive PATH). Run each as `ssh dgx '<cmd>'`:

```bash
ssh dgx 'cd ~/src/kastellan && git fetch --all -q && git checkout feat/web-research-microvm-entry && git pull -q'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --release -p kastellan-microvm-run 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-web-research-rootfs.sh 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test web_research_firecracker_egress_e2e -- --ignored --nocapture 2>&1 | tail -40'
```

Expected: `built …/web-research.ext4`; the e2e prints `test web_research_vm_reaches_proxy_with_ca_delivered ... ok` with **no `[SKIP]` line** (a `[SKIP]` means the rootfs/KVM/launcher wasn't found — investigate, do not accept as pass). The CONNECT assertion firing green proves real VM boot + force-route + vsock relay + CA delivery.

- [ ] **Step 3: On the DGX — full-workspace regression (guards the baseline).** Detached with a home-dir log (memory: `dgx-run-logs-tmp-scrubbed`):

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && setsid bash -lc "export PATH=\$HOME/.local/bin:\$PATH; cargo test --workspace > ~/dgx-wr-vm.log 2>&1; echo DONE_EXIT=\$? >> ~/dgx-wr-vm.log" </dev/null & echo launched'
```

Poll until done:

```bash
ssh dgx 'tail -3 ~/dgx-wr-vm.log; grep -c "^test result" ~/dgx-wr-vm.log 2>/dev/null'
```

Expected: `DONE_EXIT=0`; test totals ≥ the 2367/0/38 baseline plus this change's new host unit tests (the 2 Linux-gated `web_research` tests now run natively) — i.e. a higher `passed`, `0 failed`, `[SKIP]`-free. Also run workspace clippy natively:

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```

Expected: clean.

- [ ] **Step 4: Update HANDOVER.md** — add a new "Last updated" header block summarizing this feature (VM entry + resolve branch + rootfs script + DGX-gated e2e + loopback-embed caveat), the Mac + DGX verification results (real test counts from Step 3), the merged-status once the PR is up, and move the "Firecracker micro-VM entry" bullet out of "Next TODO" into completed. Note the spec/plan paths. Keep the file focused (prune older session detail if it pushes past ~500 lines). Also flip the ROADMAP web-research line: add "Firecracker VM entry MERGED …" and drop it from "Still deferred (Slice 4)".

- [ ] **Step 5: Commit the handover update**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: handover + roadmap for web-research micro-VM entry

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push
```

- [ ] **Step 6: Open the PR** (link the web-research Slice-4 arc; note the DGX gate is discharged in-band). Use `gh pr create` with a body describing the 5 changes, the Mac + DGX verification, and the loopback-embed caveat.

---

## Self-Review

**1. Spec coverage:**
- Spec change 1 (VM entry fn) → Task 1. ✓
- Spec change 2 (manifest branch + consts) → Task 2 (consts in Task 1, branch in Task 2). ✓
- Spec change 3 (rootfs script) → Task 3. ✓
- Spec change 4 (DGX-gated e2e) → Task 4. ✓
- Spec change 5 (loopback-embed caveat doc) → Task 1 Step 6 (VM entry doc-comment). ✓
- Spec "Testing" (2 unit tests + branch test + e2e) → Task 1 test, Task 2 test, Task 4 e2e. ✓
- Spec "Verification plan" Mac → Task 5; DGX → Task 6. ✓
- Spec "File-size" note → Task 5 Step 5. ✓

**2. Placeholder scan:** No TBD/TODO/"add error handling"/"similar to Task N" — every code step shows full code. ✓

**3. Type consistency:** `web_research_firecracker_entry(binary: PathBuf, image_dir: String, endpoint: &str, embed_endpoint: Option<&str>, embed_model: Option<&str>, allowlist: &[String])` — identical arg list in Task 1 definition, Task 2 call site (`&endpoint, embed_endpoint.as_deref(), embed_model.as_deref(), &allowlist`), and Task 4 e2e call (`SEARXNG_ENDPOINT, None, None, &[…]`). ✓ `base_env` signature identical in helper def (Task 1 Step 4) and both call sites (Task 1 Step 5, Step 6). ✓ Consts `USE_MICROVM_ENV`/`MICROVM_WORKER_BIN` defined Task 1 Step 3, used Task 2 Step 3. ✓ Rootfs filename `web-research.ext4` consistent across Task 1 env, Task 3 mkfs, Task 4 `firecracker_image`. ✓ In-rootfs binary path `/usr/local/bin/kastellan-worker-web-research` consistent across Task 1 const, Task 3 install, Task 4 entry call. ✓

No issues found.
