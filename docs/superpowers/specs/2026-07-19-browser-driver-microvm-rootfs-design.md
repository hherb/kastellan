# browser-driver micro-VM rootfs (slice 1 of the VM-entry arc) — design

**Date:** 2026-07-19
**Status:** approved, ready for planning
**Arc:** browser-driver Firecracker micro-VM entry — the last single-use net
worker without a VM mode.
**This spec covers slice 1 only.** Slices 2 and 3 get their own spec/plan cycles.

---

## 1. Why

`browser-driver` is the only single-use net worker still without a Firecracker
micro-VM mode. web-fetch, web-search, web-research and python-exec all have one.
Its in-jail `ProxyShim` (CONNECT-over-UDS, `workers/browser-driver/src/kastellan_worker_browser_driver/shim.py`)
already maps onto the VM sidecar tunnel, so the *mechanism* is a good fit.

### 1.1 A motivation that does NOT hold

`docs/devel/handovers/HANDOVER.md` previously pitched this work as "the named
structural fix for the open macOS [#286](https://github.com/hherb/kastellan/issues/286)
no-netns hole". **That is wrong and is corrected as part of this slice.**
Firecracker is Linux-only; #286 is a macOS Seatbelt `localhost:*` widening
(`sandbox/src/macos_seatbelt.rs:484-491`). Issue #286's own text names the
`MacosContainer` VM-netns backend ([#55](https://github.com/hherb/kastellan/issues/55))
as its fix. A Linux Firecracker entry leaves #286 exactly where it is.

The real payoff is narrower and still worth having: **uniformity** (every net
worker gains the strongest available containment tier on Linux) and the fact
that a force-routed VM has *no virtio-net device at all*
(`sandbox/src/linux_firecracker/plan.rs:255-267`), which is strictly stronger
than the bwrap private-netns path browser-driver uses today.

---

## 2. Scope

### 2.1 Goal

A `browser-driver.ext4` rootfs that:

1. boots under Firecracker on the DGX (aarch64),
2. runs the **existing, unmodified** Python browser-driver worker over the
   vsock stdio bridge, and
3. successfully launches Chromium inside the guest.

### 2.2 Explicitly out of scope (slices 2 and 3)

- `browser_driver_firecracker_entry` in `core/src/workers/browser_driver.rs`
- the `KASTELLAN_BROWSER_DRIVER_USE_MICROVM` branch in `BrowserDriverManifest::resolve()`
  (which must short-circuit the Linux lockdown-shim `Misconfigured` arm at
  `browser_driver.rs:399-402`, the same way web-fetch's VM branch short-circuits
  `discover_binary`)
- the live "render a real page through the egress sidecar from a VM" e2e

Slice 1 changes **no production Rust** except the `microvm-init` mount fix in
§4.3. Its test constructs the `ToolEntry`/`SandboxPolicy` inline, exactly as
`core/tests/web_fetch_firecracker_egress_e2e.rs:187-192` already hand-applies
what `rewrite_worker_policy` does in production.

---

## 3. Why a Dockerfile, and why that is not a runtime dependency

Every existing rootfs (`scripts/workers/microvm/build-*.sh`) is assembled from
scratch: no distro base, no root, no loop mount. Shared libraries are discovered
by running `ldd` on the worker binary (`copy_lib_closure`), then
`mkfs.ext4 -d <staging>` turns the staging dir into an image.

**Chromium breaks that pattern.** It `dlopen`s NSS modules, fontconfig backends
and SwiftShader at runtime; `ldd` cannot see any of them. Hand-curating that
closure means finding each missing library by trial-and-error against cryptic
Chromium crashes, and the list silently rots on every Chromium bump.

So slice 1 changes only the *provenance of the staging directory*:

```
docker build  →  docker create  →  docker export  →  untar to staging
                                                          ↓
                            (identical tail to all 7 existing scripts)
                            strip → install init → mkfs.ext4 -d
```

Playwright's own `playwright install --with-deps chromium` resolves the
dependency closure, which is precisely the thing it is maintained to do.

**Docker is a build-time tool only.** The runtime remains pure Firecracker with
no new dependency. Docker is already present and usable without sudo on the DGX.

### 3.1 Rejected alternatives

- **From-scratch + a curated dlopen list.** Keeps all 8 scripts uniform and
  docker-free, and keeps the trust surface explicit — but see above: `ldd`
  blindness makes it a trial-and-error tar pit that rots on each bump.
- **debootstrap distro base.** Not installed on the DGX (needs `sudo apt`),
  generally wants root, and lands in the same apt-dependency territory as the
  Dockerfile with strictly more moving parts.

### 3.2 De-risked before design (2026-07-19)

The single largest unknown — whether Playwright ships Chromium for
**linux/arm64** at all — was verified on the DGX before this spec was written.
A throwaway `ubuntu:24.04` container downloaded `chromium-1223` and
`chromium_headless_shell-1223` successfully. The only failure was Playwright's
*host-dependency validation* step, which wants exactly the apt packages that
`--with-deps` installs. aarch64 is not a blocker.

---

## 4. Components

### 4.1 `scripts/workers/microvm/Dockerfile.browser-driver`

`FROM ubuntu:24.04`; install `python3`/`python3-venv`; create a venv at
`/usr/local/lib/kastellan-browser-driver/venv`; **non-editable** install of
`workers/browser-driver` (matching `scripts/workers/browser-driver/install.sh`,
which installs non-editable so the package is copied into site-packages);
then `PLAYWRIGHT_BROWSERS_PATH=/usr/local/lib/kastellan-browser-driver/browsers
playwright install --with-deps chromium`.

The console script lands at
`/usr/local/bin/kastellan-worker-browser-driver`.

**Path choice is load-bearing.** `apply_host_mounts`
(`workers/microvm-init/src/guest.rs:110-118`) tmpfs-mounts each share anchor
(`/opt /data /srv /mnt /work /tmp`) to make the read-only root writable there.
Anything baked into the rootfs at one of those paths would be **shadowed** by
that tmpfs. `/usr/local` is not an anchor, and it matches the
`vm-worker-in-rootfs-binary-path` convention.

### 4.2 `scripts/workers/microvm/build-browser-driver-rootfs.sh`

Follows `build-web-fetch-rootfs.sh` structurally: same `KASTELLAN_MICROVM_DIR`
default (`/var/lib/kastellan/microvm`), same unwritable-dir hint, same shared
pinned `vmlinux` handling, same anchor-dir creation
(`proc sys tmp dev ro-share opt data srv mnt work run`), same journal-less
`mkfs.ext4 -q -F -O ^has_journal`.

Differences:

- staging comes from `docker export` rather than `copy_lib_closure`;
- a **strip pass** before `mkfs`: apt lists, `/usr/share/doc`, locales, ffmpeg,
  and the full `chromium-*` bundle if the worker only ever uses
  `chromium_headless_shell-*` (to be confirmed during the spike — Playwright's
  `chromium` channel selection decides this);
- `ROOTFS_MIB` is **measured, not guessed** — the spike sizes the staging dir
  first. Expected order of magnitude ~2 GiB against 256 MiB for its siblings.

`kastellan-microvm-init` is still built with cargo on the host and
`install -D -m0755`'d to `/sbin/init` in staging, exactly as today.

### 4.3 `workers/microvm-init` — pseudo-fs mounts

`mount_pseudo_fs` (`workers/microvm-init/src/guest.rs:176-197`) currently mounts
only `/proc`, `/sys`, and a tmpfs `/tmp`. Chromium additionally needs `/dev`
entries (`/dev/null`, `/dev/urandom`) and `/dev/shm`.

Add devtmpfs on `/dev` and tmpfs on `/dev/shm`, following the existing
best-effort, EBUSY-ignored idiom. The change is additive and benefits every
worker.

**Spike step 1 narrows this:** the Firecracker guest kernel may already
auto-mount devtmpfs (`CONFIG_DEVTMPFS_MOUNT`), in which case only `/dev/shm` is
needed. Verify before writing the change rather than adding a mount on
speculation.

Note that `DEFAULT_LAUNCH_ARGS` in `render.py:31-41` already passes
`--disable-dev-shm-usage`, which redirects Chromium's shared memory to `/tmp`.
That is a genuine second line of defence, but `/tmp`'s tmpfs defaults to half of
guest RAM, so the VM needs real memory headroom regardless (see §6).

---

## 5. Data flow

Unchanged from every other VM worker:

```
microvm-run  →  firecracker  →  guest init
                                  ├─ mount_pseudo_fs (+ /dev, /dev/shm)
                                  ├─ apply_host_mounts
                                  ├─ accept vsock 1024, dup2 → stdin/stdout
                                  └─ execv kastellan.worker=<in-rootfs path>
                                                    ↓
                       kastellan-worker-browser-driver (Python, unmodified)
                         main() → _apply_worker_scratch → Server.run(stdin, stdout)
                                                    ↓
                         browser.render → PlaywrightRenderer → Chromium
                                          (from PLAYWRIGHT_BROWSERS_PATH)
```

The worker is unchanged because `microvm-init` `dup2`s the vsock connection onto
fd 0/1 — the worker still just `serve_stdio()`s.

---

## 6. Guest resource budget

- **`mem_mb`**: the host entry uses 1024 (`browser_driver.rs:288-315`). A VM
  running Chromium *and* backing `/tmp` with a RAM-based tmpfs needs more;
  start at **2048** and tune down if the spike shows headroom.
- **Cmdline budget**: `MAX_CMDLINE_BYTES = 1920` (`plan.rs:135`), and env is
  hex-encoded, so **every env byte costs two**. The env set is
  `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` (JSON, the variable-length one),
  `PLAYWRIGHT_BROWSERS_PATH`, `TMPDIR=/tmp`, `HOME=/tmp`.
  `KASTELLAN_MICROVM_DIR`/`_ROOTFS` are stripped before encoding
  (`plan.rs:390`). Bail-out if it does not fit: shorten the browsers path.

`HOME=/tmp` is not cosmetic — Playwright's Node driver calls `uv_os_homedir()`,
and without it the driver dies with "Connection closed while reading from the
driver" (already documented at `browser_driver.rs:246-276`).

---

## 7. Testing

### 7.1 Hermetic pin — always runs, Mac and Linux

Feed a hand-built browser-driver VM `SandboxPolicy` through the **real**
`build_launch_plan` and assert:

- `kastellan.worker=` hex-decodes to `/usr/local/bin/kastellan-worker-browser-driver`
  (the in-rootfs path, **not** a host `target/debug` path), and
- the assembled cmdline fits under `MAX_CMDLINE_BYTES`.

This guards the `vm-worker-in-rootfs-binary-path` footgun directly. That failure
mode is a guest PID1 panic → boot loop → dispatch hanging to wall-clock, which
presents as a channel hang and has cost a debugging session before. It is worth
a test that runs on every machine.

This follows the pattern argued in
`core/tests/web_research_firecracker_broker_e2e.rs:9-20,34-40`: hermetic pins
carry the security/containment property, the `#[ignore]` DGX tier carries the
"does it actually boot" property.

### 7.2 DGX spike test — `#[ignore]`

Two assertions, in order of authority:

**(a) Primary — a direct headless-shell smoke inside the guest.** Invoke the
staged `chromium_headless_shell` binary directly and require a clean exit plus a
version string. This is deterministic: it is an exit code, not a message match,
and it isolates "the dlopen/lib closure is complete" from every other moving
part. This is the authoritative launch proof.

**(b) Secondary — one `browser.render` over vsock**, which additionally exercises
the worker, the stdio bridge and Playwright's Node driver. Note that a
launch failure and a navigation failure **both** surface as `RENDER_FAILED`
(`-32003`, `errors.py`), so the code cannot discriminate them; the discriminator
must be the message text — a navigation-class failure names a `net::ERR_*`
condition (`ERR_NAME_NOT_RESOLVED`, `ERR_PROXY_CONNECTION_FAILED`), whereas a
launch-class failure names a missing executable or a missing shared library.

Because (b) is message-based and therefore inherently brittle, **(a) is the
gate**. If the spike shows the message patterns are unstable across Playwright
versions, relax (b) to "the call returns a well-formed JSON-RPC envelope over
vsock" — which still proves boot + worker + bridge — and let (a) carry the
Chromium-launch property alone.

Gated the standard three ways: `#![cfg(target_os = "linux")]`, `#[ignore]` with
a DGX-prerequisite reason, and runtime skip-as-pass via
`LinuxFirecracker::probe` + `locate_microvm_run` + `skip_if_no_supervisor` +
`pg_bin_dir_or_skip`.

### 7.3 Unit tests

Cover the `microvm-init` mount change. The Python worker is untouched, so its
suite is unchanged.

### 7.4 Verification gate

DGX `cargo test --workspace` holds at the 2584/0/47 baseline **plus** the new
hermetic pins; `clippy --workspace --all-targets -D warnings` clean; the new
`#[ignore]` test run manually with `--nocapture`.

Per `firecracker-e2e-stale-release-launcher`, the e2e run must first
`cargo build --release -p kastellan-microvm-run` — `locate_microvm_run()`
prefers `target/release`, so a stale binary silently runs old launcher code.
And `export PATH=$HOME/.local/bin:$PATH`, since `firecracker` is off the
non-interactive SSH PATH (without it the test silently SKIP-as-passes).

---

## 8. Risks

| Risk | Mitigation |
|---|---|
| Rootfs ~2 GiB vs 256 MiB siblings | Measure staging before fixing `ROOTFS_MIB`; strip pass. DGX has 1.6 TB free. |
| Cmdline 1920-byte budget with a long allowlist | Hermetic pin asserts the budget; bail-out is a shorter browsers path. |
| Chromium `dlopen` closure still incomplete after `--with-deps` | The spike's whole purpose. Failure is loud and local to the rootfs. |
| Docker as a new build-time dep | Build-time only; already present and sudo-free on the DGX. Runtime unchanged. |
| Guest kernel may not auto-mount devtmpfs | Spike step 1 verifies before the init change is written. |

`--no-sandbox` in `DEFAULT_LAUNCH_ARGS` stops being a compromise here: inside a
micro-VM the VM is the security boundary.

---

## 9. Definition of done

1. `build-browser-driver-rootfs.sh` produces a `browser-driver.ext4` on the DGX.
2. The hermetic launch-plan pin is green on Mac and Linux.
3. The DGX `#[ignore]` spike test boots the VM, the worker answers JSON-RPC over
   vsock, and Chromium launches.
4. Workspace tests and clippy are green at the stated baseline.
5. The HANDOVER `#286` claim in §1.1 is corrected.
6. `workers/browser-driver/README.md` — which still says "slice #1 scaffold …
   raises `NotImplementedError`" and recommends `pip install -e .` — is
   corrected, since this slice makes it actively misleading. (Found while
   mapping the worker; the render has been fully implemented for some time.)
