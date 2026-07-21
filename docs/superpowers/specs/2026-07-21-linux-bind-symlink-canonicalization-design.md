# Canonicalize `..`/symlinks in the Linux bwrap + Firecracker path binds

**Issue:** [#387](https://github.com/hherb/kastellan/issues/387) (audit finding #7, `docs/security-audit-2026-07-02.md`)
**Date:** 2026-07-21
**Status:** design approved, ready for an implementation plan
**Severity:** Medium→Low (latent — policy paths are trusted-core today; this is defence-in-depth + cross-platform parity)

---

## 1. The problem

The macOS Seatbelt backend canonicalizes every policy path (resolving symlinks
and `..`) before it builds a sandbox rule. The Linux backends do not fully match
that guarantee, so audit finding #7 flagged a cross-platform asymmetry: a policy
path that *names* one location but *resolves* to another would bind/stage what it
resolves to, not what it names.

Half of #387 already shipped. `validate_linux_bind_path`
([`sandbox/src/lib.rs`](../../../sandbox/src/lib.rs)) rejects any path that is
not absolute **or** contains a `..` (`ParentDir`) component, and both Linux
backends call it up front — bwrap in `spawn_under_policy`, the Firecracker plan
builder in `build_launch_plan`. Because that runs *before*
`non_anchor_top_level`, the issue's `/opt/../etc` example is already rejected.
That is the "deterministic, config-independent half" the docstring names.

The **symlink half** is the residual, and it differs per backend:

| Backend | Today | Residual gap |
|---|---|---|
| **bwrap** | absolute + no-`..` only; then `--ro-bind-try`/`--bind` the raw path | **No symlink resolution at all.** `--ro-bind-try /opt/link` where `link → /etc` silently binds `/etc` read-only, with no audit trail of the real target and a TOCTOU window (bwrap resolves the source at bind time, after our check). |
| **Firecracker** | `copy_tree` ([`images.rs`](../../../sandbox/src/linux_firecracker/images.rs)) already skips *within-tree* symlinks that resolve to a directory or escape the `fs_read` root (issues #370 + #387) | The `fs_read` **root itself** being a symlink: `root = canonicalize(src)` is used to scope the escape check, so the root's *own* escape is not checked against the anchor allowlist. A root symlink to a **file** outside the anchors (e.g. `/opt/link → /etc/shadow`) is copied into the RO image; `non_anchor_top_level` only ever saw the lexical `/opt` (an anchor → pass). |

The escape is only reachable by a caller who can influence a policy path, which
today is trusted-core — hence Med→Low. The value is closing the audit finding
with the **same guarantee on both OSes**, so a future untrusted-path caller (if
one is ever added) is contained identically on Linux and macOS.

## 2. Goal & non-goals

**Goal.** Give the two Linux backends the same symlink-resolution guarantee the
macOS Seatbelt backend already has, from a single shared implementation, and
re-run the existing containment checks on the *resolved* path.

**Posture: resolve, not reject** (operator decision). Canonicalize the source and
proceed with the resolved path — exactly what macOS does. A symlink that escapes
a Firecracker anchor is then rejected by the *existing* allowlist on its resolved
top-level; a legitimate symlinked standard dir (e.g. an operator-symlinked
scratch root, or `/var` being a symlink) keeps working, as it does on macOS.
Rejecting on *any* symlink was considered and declined: it diverges from macOS
(so it is not "parity") and breaks legitimate symlinked directories.

**Non-goals.**
- Not touching the pure lexical guard — it stays as the first pass.
- Not adding *new support* for symlinked `fs_read` roots on Firecracker; only
  guaranteeing they cannot **escape** the anchor set.
- Not changing the threat model. Policy paths remain trusted-core; this is
  defence-in-depth.
- Not resolving the inherent TOCTOU of canonicalize-then-use (see §7).

## 3. Design

### 3.1 Shared canonicalization primitive (`lib.rs`)

Lift `canonicalize_one` out of
[`macos_seatbelt.rs`](../../../sandbox/src/macos_seatbelt.rs) into `lib.rs` as
`pub(crate) fn canonicalize_one(&Path) -> Result<PathBuf, SandboxError>`. Its
behaviour is unchanged:

- `std::fs::canonicalize(p)` on success.
- On `NotFound` (a not-yet-created scratch dir or socket file), canonicalize the
  **parent** and reattach the file name — so symlinks in the parent chain are
  still resolved even before the leaf exists. If the parent is also `NotFound`,
  fall back to the original path.
- Any other `io::Error` (e.g. `PermissionDenied` on a parent) propagates as a
  `SandboxError::Backend`, so a caller never silently emits a rule/bind for an
  unresolved path.

Seatbelt's `canonicalize_policy_paths` is updated to call `crate::canonicalize_one`
(one-line change; it keeps its own field selection, which canonicalizes
`guest_mount` — correct on macOS, wrong on Linux, see §3.4). The two Linux
backends call the shared primitive too. **One copy** of the fallback logic — the
single source of the guarantee on both OSes, avoiding the duplication the project
is repeatedly burned by.

### 3.2 The lexical guard stays as the first pass

`validate_linux_bind_path` (absolute + no-`..`) is unchanged and still runs
**first**, on every host-source and guest-side path as it does today. It is pure,
FS-independent, and fails closed deterministically, which keeps its unit tests
pure. Canonicalization is a **second** pass layered on top: lexical belt,
symlink suspenders.

### 3.3 bwrap backend

In `spawn_under_policy`, after the existing lexical validation loop,
canonicalize each **host-source** field with `canonicalize_one` and thread the
resolved path into the bind as `canonical-src → original-dest`:

- `push_bind` gains a `dest` parameter (it currently emits `flag src src`).
  Callers pass `(canonical, original)`.
- For a non-symlink path, `canonical == original`, so the emitted argv is
  **byte-identical** to today (`--ro-bind-try /etc/ssl /etc/ssl`) — no
  regression for the common case, which the existing argv tests pin.
- For `/opt/link → /etc`: emits `--ro-bind-try /etc /opt/link`. The worker still
  opens `/opt/link` inside the jail and gets `/etc`, but we pass the **resolved
  literal** as the source, so bwrap cannot be raced between our check and its
  bind (TOCTOU-safe), and the argv/audit shows the real target.

Host-source fields on bwrap: `fs_read`, `fs_write`, `proxy_uds`, `broker_uds`,
`persistent_store.host_backing`.

### 3.4 Firecracker backend (minimal, targeted)

`copy_tree`'s within-tree symlink handling is left untouched. The one residual —
a symlinked `fs_read` root — is closed by canonicalizing the root and running the
**existing** anchor check on the resolved top-level. In `build_launch_plan`,
where `non_anchor_top_level(p)` is applied to each `fs_read` path, apply it to
`canonicalize_one(p)` instead:

- `/opt/link → /etc/shadow`: resolves to `/etc/shadow`, top-level `etc` →
  `non_anchor_top_level` returns `Some("etc")` → **rejected**. (Closes the
  file-leak.)
- `/opt/link → /etc` (a dir): resolves to `/etc`, top-level `etc` → **rejected**
  — an upgrade from today's "silently skipped by `copy_tree` with a warn" to a
  loud, actionable reject.
- `/opt/link → /data/models` (`/data` is an anchor): resolves within the anchor
  set → accepted, staged as today.

Firecracker canonicalizes **`fs_read` only** — see the field table below.

#### Per-backend field scope (the host-vs-guest distinction)

Only **host-source** paths are canonicalized. Resolving a **guest-side** path
against the *host* filesystem would be meaningless, so those keep the lexical
check only:

| Field | bwrap | Firecracker |
|---|---|---|
| `fs_read` | canonicalize (host, bound `--ro-bind`) | canonicalize (host, staged into image) |
| `fs_write` | canonicalize (host, bound `--bind`) | **no** — a guest mountpoint, no host content |
| `proxy_uds` / `broker_uds` | canonicalize (host, bound `--bind`) | **no** — rides vsock, not a bind |
| `persistent_store.host_backing` | canonicalize (host dir) | out of scope (mkfs-created image; edge, no symlink surface) |
| `persistent_store.guest_mount` | **no** — in-jail destination | **no** — in-VM path |

## 4. Testing (TDD)

Where each test *runs* matters, because `lib.rs` compiles on both OSes while the
two backends are `cfg(target_os = "linux")`-gated (DGX-authoritative). Getting
this wrong is the `cfg-linux-e2e-deadcode-dgx-clippy` trap.

- **Pure unit tests** for `validate_linux_bind_path`: unchanged.
- **`canonicalize_one` real-FS tests (new, both hosts).** `canonicalize_one`
  lives in `lib.rs` and compiles on macOS + Linux (like `validate_linux_bind_path`),
  so its tests run on both. Create a `tempdir`, make a symlink inside it, and
  assert: a symlink is **resolved**; the `NotFound`→parent fallback resolves a
  parent symlink for a not-yet-created leaf; a permission error propagates. These
  assert *resolution equivalence* (resolved == a second independent
  canonicalize), not anchor semantics, so they are robust to macOS resolving a
  tempdir through `/private/...` and `/var/folders → /private/var/folders`.
- **bwrap argv test (new, DGX-authoritative).** `linux_bwrap` is `cfg(linux)`, so
  this runs on the DGX: a `/anchor/link → /outside` source is emitted as
  `--flag <resolved> <original-dest>` (resolved source, original in-jail path),
  and a non-symlink path is byte-identical to today (the existing argv tests pin
  the common case).
- **Firecracker anchor test (new, DGX-authoritative).** `plan.rs` is `cfg(linux)`;
  on Linux a `tempdir` lands under `/tmp` (an anchor), so a link *within* the
  tempdir resolves within the anchor set (accepted) while a link to `/etc`
  resolves out (rejected). This test is **not** attempted on macOS, where a
  tempdir canonicalizes to `/private/...` (outside the anchor set) and would
  reject for the wrong reason — the anchor semantics are a Linux-guest concept.
- **Negative-case discipline.** Each new security assertion is first proved to
  **fail** against the un-hardened code path, then the fix restored (the #479
  house rule). Both hosts are checked for hygiene after any scripted edit over
  the `cfg`-gated files — the DGX clippy gate covers the Linux direction, the Mac
  the reverse (`cfg-linux-e2e-deadcode-dgx-clippy`, both directions).

## 5. Verification

- **DGX (real bwrap, native aarch64):** `cargo test --workspace` and
  `clippy --workspace --all-targets -- -D warnings`, both exit 0, **0 `[SKIP]`**.
  Starting baseline: **main is 2629 / 0 / 50** at `4c03929f` (#386's +9 merged).
- **Mac:** the lifted primitive is a blast-radius change to the macOS Seatbelt
  backend — the Seatbelt profile tests and the four `browser_driver`/Seatbelt
  `--ignored` e2es must stay green.
- New always-running test count is expected to rise by exactly the count of new
  unit tests added (record the delta in the handover).

## 6. Files touched

- `sandbox/src/lib.rs` — add `pub(crate) canonicalize_one` (+ its tests).
- `sandbox/src/macos_seatbelt.rs` — `canonicalize_one` → `crate::canonicalize_one`
  (delete the local copy; `canonicalize_policy_paths` calls the shared fn).
- `sandbox/src/linux_bwrap.rs` — canonicalize host-source fields; `push_bind`
  gains a `dest` param; bind `canonical-src → original-dest`.
- `sandbox/src/linux_firecracker/plan.rs` — `non_anchor_top_level` on the
  canonicalized `fs_read` root.

## 7. Risks & documented limits

- **TOCTOU.** Canonicalize-then-use is not atomic. bwrap is made TOCTOU-safe by
  passing the resolved literal as the bind source. On Firecracker the anchor
  check canonicalizes and `copy_tree` canonicalizes again at copy time, so a
  swap between them is caught by the copy-time check; documented, not sold as
  atomic — the same TOCTOU class macOS accepts.
- **Blast radius on macOS.** Lifting `canonicalize_one` touches a working
  backend with passing e2es; mitigated by running the Mac Seatbelt suite (§5).
  The move is mechanical and the field selection is unchanged.
- **Firecracker guest-path note.** A within-anchor symlinked `fs_read` root is
  staged/mounted as today (a symlink to a dir is skipped-with-warn by
  `copy_tree`; a symlink to a file is copied). We are not *newly supporting*
  such roots, only guaranteeing they cannot escape — so no in-guest remap
  behaviour is introduced.
