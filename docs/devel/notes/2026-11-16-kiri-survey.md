# Cross-project study: **Kiri** as a lightweight Docker replacement

**Date:** 2026-11-16
**Status:** Investigation only. No code changes. **Verdict: do not adopt** as a dependency or as the
recommended runtime for SearxNG. An operator may still use it on a Mac dev box: it needs no
kastellan change.
**Question:** Could Kiri replace Docker/podman for the containers kastellan's scripts run, SearxNG
first?

> Source: [`libracoder/kiri`](https://github.com/libracoder/kiri) @ `260cf66` (2026-11-16),
> **Apache-2.0** (guest kernel GPL-2.0). About 71k lines of Rust in 7 crates. Read: README,
> `docs/{features,development}.md`, `RELEASES.md`, `SECURITY.md`, `packaging/linux/*`,
> `crates/kiri-init/src/linux/{container,seccomp}.rs`, `crates/kiri-vmm/src/lib.rs`,
> `crates/kiri/src/{update.rs,diag/report.rs}`. Source was read only; nothing was built or run.

**Licence:** Apache-2.0 is compatible with AGPL-3.0, so the licence does not block it. It is out
for the reasons in §2.

---

## 1. What it is

Kiri is a Docker Desktop / OrbStack replacement. It serves the **Docker Engine API** and bundles the
`docker` CLI, Compose and buildx, so existing `docker run` lines work unchanged. There is no runc,
shim or dockerd. A ~900 KB Rust agent starts each container with one `clone3` into new namespaces
and a cgroup (`CLONE_INTO_CGROUP`), then sets up overlayfs, `pivot_root`, capabilities and seccomp.
Networking uses hand-written rtnetlink and nf_tables messages.

- **macOS:** one small Linux VM (Virtualization.framework, custom 10 MB kernel) with the agent as
  PID 1. The daemon is per-user (LaunchAgent). The VM sleeps after 60 s idle.
- **Linux:** no VM. The agent runs **as root on the host kernel** (`crates/kiri-vmm/src/lib.rs`: "as
  a plain root process beside the daemon"), from a **system** unit (`packaging/linux/kiri.service`,
  no `User=`, `Delegate=yes`).
- Extras unrelated to our use: a Kubernetes (k3s) cluster, "Linux machines", USB passthrough,
  `*.kiri.local` names with its own CA, and **Kiri Share**, a public HTTPS tunnel to `runkiri.cloud`.

Its headline numbers (2.5 ms container start through the API, ~15 MB daemon) describe **short-lived**
containers.

## 2. Why not, for kastellan

1. **It is two weeks old.** First commit 2026-11-02; 106 commits; 90 of them by one author; releases
   0.1.x to 0.2.1. Releases are built in a **private** repository and are not reproducible
   (`RELEASES.md`). The public CI runs CodeQL, perf and UI tests. Its ~350-check `tests/e2e.sh` is
   run by hand. We keep that kind of dependency out of the containment path (see the Obscura
   decision, #707).

2. **On Linux it is a rootful daemon with no rootless mode.** Membership of the `kiri` group is
   "root-equivalent, exactly like the `docker` group" (their `features.md`). Containers get **no
   user namespace**: `CLONE_NEWUSER` is absent from the clone flags (`container.rs:304`) and is
   blocked by the seccomp filter, so a container's root is the host's root. Kastellan's whole
   deployment runs under `systemd --user` with no root daemon. Its invariant bounds a compromise to
   the agent's own OS user (`docs/threat-model.md`). If the agent user were ever added to `kiri`
   for convenience, that bound would be gone. Rootless podman, which `setup-searxng.sh` already
   supports, has none of these problems.

3. **Its seccomp filter is a deny list.** `seccomp.rs` says it is "a deny list rather than Docker's
   allow list", chosen for speed. A deny list lets through any syscall it doesn't list, including
   ones added in later kernels. Docker and podman fail closed there. Kiri fails open.

4. **Network calls to its vendor, all off or opt-in for our use but present:** a self-updater
   (`download.runkiri.com` feeds; SHA-256 and, on the Mac, Developer-ID checks), crash upload to
   `POST /v1/reports` (only if the user turned crash reports on), and Kiri Share. On Linux it also
   claims `/run/docker.sock` whenever no other engine holds it.

5. **The benefit is for workloads we don't have.** SearxNG is one container that runs all the time.
   A 2.5 ms start and a sleeping VM save nothing there. SearxNG is also **outside the trust
   boundary**: the jailed `web-search`/`web-research` workers reach it only through the trusted
   `search-broker` on loopback (`scripts/web-search/dgx-search-broker-cutover.md`). Which runtime
   hosts it changes the host's attack surface, not kastellan's. That is a reason to pick the
   runtime with the smallest privilege, not the fastest start.

## 3. What to do instead, by host

| Host | Recommendation | Why |
| --- | --- | --- |
| **Linux (DGX)** | **Rootless podman**, ideally as a quadlet `.container` unit under `systemd --user` | User namespaces, no root daemon, and it fits the supervisor model we already use. `setup-searxng.sh` prefers podman when both are on PATH (since 2026-11-27; `KASTELLAN_SEARXNG_RUNTIME` overrides). |
| **macOS** | Apple `container` (already a dependency for python-exec's micro-VM backend, Apache-2.0), or whatever the operator already runs | Adds no new dependency. Needs a check that the installed `container` version publishes ports (`-p 127.0.0.1:8888:8080`). If it doesn't, Docker Desktop/OrbStack/Kiri all work unchanged via `docker`. |
| **Either, longer term** | Run SearxNG **without a container**: a uv venv, launched under our own bwrap / Seatbelt policy as a supervised sidecar, egress through the egress proxy | Removes the container runtime for this service. It is Python outside a worker, so it needs its own design note; it is not a quick swap. |

The other Docker users in the tree are the same story. `setup-conduwuit.sh` already takes
docker/podman or a native binary. `build-browser-driver-rootfs.sh` only needs
`docker build` + `export` at build time. Podman does both. Kiri builds through BuildKit in a
container (buildx `docker-container` driver) until its built-in builder lands.

## 4. When to look again

If Kiri ships a rootless Linux mode with user-namespaced containers and an allow-list seccomp
profile, and has a year of public releases, it is worth a second look for the macOS dev path. Until
then nothing in kastellan should depend on it, and the docs should not recommend it.
