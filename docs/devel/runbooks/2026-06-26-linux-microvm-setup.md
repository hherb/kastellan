# Runbook — Linux Firecracker micro-VM backend setup (python-exec)

**Status:** Slice 1, verified live on the DGX 2026-06-27.

The optional `SandboxBackendKind::FirecrackerVm` backend boots a worker inside a
throwaway Firecracker guest kernel — defense-in-depth **on top of** bwrap +
seccomp + Landlock + cgroup, with `mem_mb` enforced by KVM. Slice 1's only
consumer is `python-exec` (`KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`), `Net::Deny`.

This is **opt-in** and **Linux-only**. Without the setup below, the backend's
`probe()` fails closed and the worker stays on bwrap.

## One-time host setup (three steps, by privilege)

Run from the repo root on the target host (the DGX). The order matters: the
privileged step provisions the image dir the unprivileged steps write.

```sh
# 1. Privileged (sudo): load + persist vhost_vsock, ACL-grant the worker user
#    rw on /dev/vhost-vsock, and provision /var/lib/kastellan/microvm.
#    /dev/kvm is usually already accessible — add --kvm if not.
sudo ./scripts/linux/install-firecracker-vsock.sh            # [--user <name>] [--kvm]

# 2. Per-user (NOT sudo): install the pinned firecracker v1.16.0 to ~/.local/bin.
./scripts/workers/microvm/install-firecracker.sh

# 3. Per-user: fetch the pinned guest kernel + build the rootfs (worker + init +
#    python3 + its lib closure) into /var/lib/kastellan/microvm.
./scripts/workers/microvm/build-rootfs.sh
```

Notes:
- The scripts reject `sh`/`sudo` mis-invocation with a clear message; invoke them
  exactly as shown (`./script`, and `sudo` **only** on step 1).
- `~/.local/bin` must be on `$PATH` (step 2's binary + the probe's PATH lookup).
- To build into a user-writable dir instead of `/var/lib/...`, set
  `KASTELLAN_MICROVM_DIR=$HOME/.local/share/kastellan/microvm` on step 3 **and**
  in the kastellan service env so the backend looks there too.

## Enable

Set both env vars wherever the python-exec worker should run in the micro-VM:

```sh
KASTELLAN_PYTHON_EXEC_ENABLE=1
KASTELLAN_PYTHON_EXEC_USE_MICROVM=1
```

(`KASTELLAN_MICROVM_DIR` defaults to `/var/lib/kastellan/microvm`.)

## Verify

`kastellan-microvm-run` must be on `$PATH` (or built into the workspace `target/`
dir the e2e auto-discovers). Then:

```sh
cargo build -p kastellan-microvm-run     # the launcher the backend spawns
PATH="$HOME/.local/bin:$PATH" \
KASTELLAN_PYTHON_EXEC_ENABLE=1 KASTELLAN_PYTHON_EXEC_USE_MICROVM=1 \
KASTELLAN_MICROVM_DIR=/var/lib/kastellan/microvm \
  cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --ignored --nocapture
```

Expected: 4 passed in well under a second — `print(6*7)→42` round-trips,
the 512 MiB cap is KVM-enforced (a ~900 MiB allocation raises `MemoryError`),
`Net::Deny` blocks an outbound socket, and a >64 KiB params payload rides the
in-VM `/tmp` file channel. The suite `[SKIP]`s cleanly (probe fail) if the host
setup is incomplete.

## How it works (one paragraph)

`LinuxFirecracker::spawn_under_policy` renders a per-spawn Firecracker config
(unique vsock UDS + guest CID) and spawns `kastellan-microvm-run` as the worker
`Child`. The launcher boots Firecracker (kernel console → log file, never
stdout), connects the guest over **host-initiated hybrid vsock** (dial the base
UDS, send `CONNECT <port>\n`, await `OK`), and bridges its stdin↔vsock↔stdout —
flushing each relayed chunk so JSON-RPC responses reach the host immediately.
Inside the guest, PID1 `kastellan-microvm-init` mounts proc/sys/tmp, accepts the
vsock connection, `dup2`s it onto fd 0/1, and execs the **unchanged**
`serve_stdio` python-exec worker. Teardown is driven by the host closing the
launcher's stdin; the launcher kills Firecracker, so no VM is orphaned.

## VMM confinement (slice 5a, default-ON)

Since slice 5a, the VMM itself (the `kastellan-microvm-run` launcher + the
`firecracker` process it spawns) runs **inside an unprivileged `bwrap` jail + a
`systemd-run --user --scope` cgroup** — defense-in-depth around the hypervisor,
on top of the guest-kernel boundary. This is **on by default** for every
Firecracker VM worker and needs no extra setup beyond what this runbook already
requires, because it reuses the same layers the bwrap workers use:

- the unprivileged-userns **AppArmor profile**
  (`sudo scripts/linux/install-bwrap-apparmor-profile.sh`, Ubuntu 24.04+), and
- a live **`systemd --user`** session (`loginctl enable-linger $USER` on headless
  hosts).

If either is missing, the firecracker `probe` **fails closed** and names both
fixes — the VM worker refuses to spawn rather than run the VMM unconfined. To run
VMs **without** host-side VMM confinement (e.g. a host that can't provide the
AppArmor profile), set the opt-out:

```sh
KASTELLAN_MICROVM_CONFINE_VMM=0   # default unset = confined
```

Note: under confinement the host cgroup `MemoryMax` equals the worker's `mem_mb`
(the guest RAM), covering firecracker's guest-RAM mapping plus VMM overhead — a
tight ceiling that is verified-passing for the 512 MiB python-exec worker. A
future worker with a much larger guest RAM may want headroom for the VMM process
itself (`mem_mb` reflects guest RAM, not the VMM-process budget).

## Reversing the host setup

```sh
sudo rm -f /etc/udev/rules.d/99-kastellan-microvm.rules \
           /etc/modules-load.d/kastellan-vsock.conf
sudo udevadm control --reload
sudo rm -rf /var/lib/kastellan/microvm    # the built rootfs + kernel
rm -f ~/.local/bin/firecracker
```
