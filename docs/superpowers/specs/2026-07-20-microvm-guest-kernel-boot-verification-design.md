# Guest-kernel integrity at VM boot, not just at build time

**Issue:** [#479](https://github.com/hherb/kastellan/issues/479)
**Date:** 2026-07-20
**Builds on:** [#471](https://github.com/hherb/kastellan/issues/471) / PR [#478](https://github.com/hherb/kastellan/pull/478)
**Status:** design approved, ready for an implementation plan

---

## 1. The problem

PR #478 made `fetch_guest_kernel` sha256-verify the micro-VM guest `vmlinux`
on every rootfs build, including a copy already sitting on disk. That closed
the fetch-time gap and the reuse-unchecked gap.

Verification is still **build-time only**. At boot,
`sandbox/src/linux_firecracker.rs:64` takes the kernel unconditionally:

```rust
kernel_path: dir.join("vmlinux"),
```

Nothing re-checks it. The window between "verified during some build" and
"booted, possibly months later, hundreds of times" is entirely unguarded.

That window matters because of who owns the file:

```
drwxr-xr-x 2 hherb hherb  /var/lib/kastellan/microvm/
-rw-rw-r-- 1 hherb hherb  vmlinux
```

`docs/threat-model.md` assumes a worst-case compromise reaches *exactly* the
agent's own OS user. So an attacker at that level can overwrite `vmlinux` and
every subsequent micro-VM boots a kernel of their choosing — and the guest
kernel is the thing enforcing the containment boundary the rest of the model
rests on. No build need ever run again for this to hold.

The ownership is not an oversight: `install-firecracker-vsock.sh` chowns the
dir to the worker user *deliberately*, so the eight unprivileged
`build-*-rootfs.sh` scripts can write it without root. Any fix has to respect
that this was a considered decision, not accidentally undo it.

## 2. Decisions

Three decisions were taken during design, each with a rejected alternative
worth recording.

### 2.1 Do both halves, not one

Verification detects tampering; ownership prevents it. Neither alone is
adequate here:

- **Ownership alone** is enforced entirely outside the codebase. Nothing
  in-tree fails if an install predates the change, or if an operator points
  `KASTELLAN_MICROVM_DIR` at a directory they control.
- **Verification alone** is inherently TOCTOU (see §4.3) and leaves the write
  primitive in place.

Together they cover each other's gap. This matches the issue's own preference.

### 2.2 Protect the kernel, not the whole directory

**Chosen:** sticky bit on the image dir, with **root owning the directory,
its parent, and `vmlinux`**, and the worker's group holding write on the
image dir so builds stay unprivileged.

> The original wording here was "sticky bit + `vmlinux` **alone** owned by
> root", which is wrong and was implemented before being caught — see the
> correction in §3.1. Root must own the directory too.

**Rejected:** root-owning the entire directory (with either staged builds or
`sudo`-run builds). Two reasons:

1. **The rootfs images do not need it.** A tampered rootfs is not an
   escalation — the guest userland is already the thing we assume is hostile,
   and the VM is what contains it. The kernel is categorically different: it
   *is* the containment mechanism. Protecting both would mean adding a
   privileged step to every image build to defend an artefact that gains an
   attacker nothing.
2. **The privileged surface goes the wrong way.** `sudo`-running the builds
   would execute `docker export`, `debootstrap` and `mkfs` as root on every
   rebuild — a large new privileged surface introduced to fix a problem in one
   16 MB file.

### 2.3 Duplicate the sums, enforce the duplication in CI

**Chosen:** the expected sums are consts in Rust, cross-checked against
`scripts/workers/microvm/lib/guest-kernel.sh` by a test.

**Rejected — `build.rs` parsing the shell file:** zero duplication, but
`kastellan-sandbox` is published to crates.io and the script lives outside the
crate directory, so `cargo publish` verification would fail. A vendored
fallback copy reintroduces the duplication anyway.

**Rejected — inverting, so Rust owns the pin and bash greps it out:** genuinely
one source of truth, but it relocates the pin away from the scripts directory
one day after #471 deliberately put it there, and makes the provisioning path
depend on parsing a file inside a Rust crate.

The chosen shape keeps `guest-kernel.sh` as the human-facing place an operator
edits on a version bump, and makes the duplication CI-enforced rather than
hoped-for — the same pattern #471 already established with
`kernel_pin_is_the_only_place_the_kernel_url_appears`.

## 3. The boundary: sticky dir + root-owned kernel

`scripts/linux/install-firecracker-vsock.sh` (already root-only) changes in
three ways:

1. `/var/lib/kastellan/microvm/` becomes **`root:<worker-group>` mode `1775`**
   — root-owned, group-writable, and **sticky**.

   > **Correction (2026-07-21, found in the Task 3+4 review).** This section
   > originally specified mode `1755` with the directory *still owned by the
   > worker user*. That does not work, and the "why the sticky bit is
   > load-bearing" paragraph below stated the reason incorrectly. `unlink(2)`
   > refuses removal from a sticky directory only when the process's UID "is
   > neither the UID of the file to be deleted **nor that of the directory
   > containing it**" — **two** exemptions. A worker-owned directory satisfies
   > the second one, so the agent could still `rm` root's `vmlinux` and
   > replace it, and the ownership half would have been void while appearing
   > correct. Root must own the directory as well; the worker's group gets
   > write so unprivileged rootfs builds still work.
2. The script takes over fetching the guest kernel: it sources
   `guest-kernel.sh`, calls `fetch_guest_kernel` (unchanged — it already
   verifies), then `chown root:root` + `chmod 0644` the result.
3. The eight `build-*-rootfs.sh` scripts stop fetching. Their
   `fetch_guest_kernel "$OUT_DIR"` call is replaced by a new
   `require_guest_kernel "$OUT_DIR"` in the same shared file, which verifies
   the kernel is present and matches and on failure tells the operator to run
   the install script. `guest-kernel.sh` thus gains a second entry point and
   keeps `fetch_guest_kernel` for the install script; only the set of callers
   changes. The build scripts still source the shared file, so #478's
   `kernel_pin_is_the_only_place_the_kernel_url_appears` drift test continues
   to hold unmodified.

   > **This item was briefly dropped during planning and then restored
   > (2026-07-21, final review).** The argument for dropping it was that
   > `fetch_guest_kernel` already early-returns on a verified kernel without
   > writing, so a root-owned kernel is no obstacle to an unprivileged build.
   > That is true for the present-and-good and present-and-bad paths, and
   > **false for the absent path** — the one that matters. The image dir is
   > group-writable so builds can manage their own `*.ext4`, which also means
   > a build can *create* a new entry: if `vmlinux` were ever missing, a
   > build would rename its download into place and leave an **agent-owned**
   > kernel. No unlink of root's file, nothing failing, and the ownership
   > half silently void from then on. Verify-but-never-create removes that
   > path.

   Sticky-bit compatibility: a build creating or truncating **its own** rootfs
   image is unaffected — `+t` restricts only removal and rename by non-owners,
   and the agent user owns every `*.ext4` in that directory.

4. A new `scripts/workers/microvm/fetch-guest-kernel.sh` covers the
   alternative layout the build scripts document
   (`KASTELLAN_MICROVM_DIR="$HOME/.local/share/kastellan/microvm"`), as a
   deliberate operator action. Root does not manage that directory, so it
   carries **no ownership protection at all** — only the boot-time hash. The
   script says so, and refuses to run against the default dir.

5. Two pre-existing hazards the installer must handle, both found in the final
   review: a `vmlinux` that is already a **symlink** (`[ -f ]`, `chown` and
   `chmod` all follow links, so the fetch would verify through it and the
   chown would retarget its destination while the agent-owned *link* survived)
   is removed rather than followed; and the **parent** `/var/lib/kastellan` is
   root-owned `0755`, since unlink/rename permission on the image dir is
   governed by its parent. The installer also `stat`s the kernel's uid before
   claiming success.

**Why the sticky bit is load-bearing, and why root must own the directory.**
POSIX directory write permission alone permits `unlink()` and `rename()` of
*any* entry in that directory, regardless of the file's own owner and mode.
Root-owning `vmlinux` inside a group-writable directory without `+t` would
therefore stop nothing: the agent could simply remove it and create its own.

`+t` narrows removal and rename to **either** the file's owner **or the
directory's owner** (or root). Both exemptions matter: making the worker the
directory's owner would hand it the second one and defeat the whole
arrangement. So the directory is `root:<worker-group>`. The worker is neither
`vmlinux`'s owner nor the directory's, so it cannot unlink or rename the
kernel; group write still lets it create and replace its own rootfs images —
which is what keeps builds unprivileged — because it owns those.

Net effect on operator UX: rootfs builds stay unprivileged; a guest-kernel
version bump costs one `sudo` run of a script that already required `sudo`.

## 4. Boot-time verification

### 4.1 A dual-platform module

New `sandbox/src/guest_kernel_pin.rs`, deliberately **not**
`#[cfg(target_os = "linux")]`-gated even though its only caller is.

This applies #471's own stated lesson: a fail-closed check exercised on one
host is half-verified. Everything in this module — the arch→sum table, hashing
a file, the verdict — needs no KVM, no Firecracker, and no Linux. Leaving it
inside the `cfg(linux)` island would make the dev Mac structurally unable to
run its tests, which is precisely the trap recorded in
`cfg-linux-e2e-deadcode-dgx-clippy`.

### 4.2 Pure / IO split

Following the house preference for pure functions in reusable modules:

- `expected_sha256(arch: &str) -> Option<&'static str>` — pure table lookup.
  Returns `None` for anything unrecognised rather than a default, so an
  unknown architecture cannot degrade into an unverified boot.
- `verify_kernel(path: &Path, expected: &str) -> Result<(), KernelPinError>` —
  reads and hashes the file, compares. The only IO. Testable against temp
  files on both hosts.

`LinuxFirecracker::spawn_under_policy` calls it immediately after
`resolve_image` and before `build_launch_plan`, so a failure costs nothing and
happens before any run-dir, config or image work.

New dependency: `sha2` (already a workspace dependency; MIT/Apache-2.0, so
AGPL-compatible).

### 4.3 Stated limitation: this is TOCTOU

The check hashes the file, then Firecracker opens it separately a moment
later. An attacker who can write the file between those two events wins. This
is documented in the module doc comment rather than left implicit.

What it buys is still substantial: the exposure shrinks from *months and
hundreds of boots* to microseconds. What actually closes the hole is §3 —
removing the write primitive. The two halves are complementary, and neither
should be described as sufficient alone.

Closing the TOCTOU properly would mean hashing through an already-open fd and
handing that same fd to Firecracker, which the Firecracker config-file
interface does not accommodate. Not attempted.

### 4.4 Failure mode

Fail closed in every case — mismatch, unreadable file, or unrecognised
architecture — returning `SandboxError::Backend` so no VM starts.

**No bypass environment variable.** A `KASTELLAN_MICROVM_SKIP_KERNEL_VERIFY`
would be exactly the "spawn unsandboxed escape hatch" `CLAUDE.md` forbids,
applied to the single artefact that defines the containment boundary.

## 5. Sum source of truth

The two arch sums become consts in `guest_kernel_pin.rs`. A new test in
`kastellan-tests-common` parses `scripts/workers/microvm/lib/guest-kernel.sh`
and asserts both pairs match, sitting alongside the existing
`kernel_pin_is_the_only_place_the_kernel_url_appears` and reusing its
`GUEST_KERNEL_LIB` const.

`linux-check.yml` already runs that crate's tests (added by #478), so a bump
that updates one place and not the other fails on the **PR** rather than on an
operator's occasional DGX run — which is the case least likely to be caught
otherwise.

## 6. Testing

**Hermetic unit tests, both hosts** (`guest_kernel_pin.rs`): correct file
verifies; corrupted file rejected; missing file rejected; unknown arch yields
`None` and refuses. These run on the dev Mac and on the DGX.

**Cross-check test** (`tests-common`): consts match `guest-kernel.sh`, per §5.

**Live exercise, free:** every Firecracker e2e already points
`KASTELLAN_MICROVM_DIR` at the real image dir holding the genuine pinned
kernel, so the full DGX `cargo test --workspace` exercises the real boot-time
path across all fifteen e2e files without a single new live test.

**Negative case, per house practice:** corrupt the const, confirm the live e2e
fails loudly, restore. A fail-closed check that has never been observed to
fail is not yet known to be load-bearing.

**Script changes:** verified on the DGX by running the install script and a
rootfs build against the real image dir, confirming `vmlinux` ends up
`root:root 0644` in a root-owned `1775` directory, that a rootfs build succeeds
unprivileged, and that the agent user genuinely cannot unlink the kernel.

## 7. Cost gate

The hash costs a ~16 MB (aarch64) / ~40 MB (x86_64) read per VM start. This
will be **measured on the DGX before the work is considered done**, against
the single-use spawn path (the frequent one).

If it is noise against VM boot — likely, given ARM crypto extensions — no
caching is added. If it proves material, the decision comes back to the
operator rather than being resolved silently with an mtime/inode cache: such a
cache reintroduces precisely the trust-what-is-on-disk pattern this work
exists to remove.

## 8. Out of scope

- **Rootfs image verification.** Per §2.2 — no recorded sum exists (images are
  built locally and vary per build), and a tampered rootfs is not an
  escalation. The sticky bit does not protect them either. Accepted knowingly,
  not overlooked.
- **[#386](https://github.com/hherb/kastellan/issues/386)** — the Firecracker
  binary and the Matrix homeserver binary are still fetched unchecked. Same
  family, separate issue; it can now reuse `verify_sha256`.
- **Closing the TOCTOU window** (§4.3).
