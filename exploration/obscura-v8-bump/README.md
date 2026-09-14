# Obscura V8 security bump

> **Re-assessed 2026-09-14.** The original version of this note (2026-06) got
> several things wrong; the corrections are marked ⚠️ below. Read those before
> trusting anything here.

## What Obscura is

[Obscura](https://github.com/h4ckf0r0day/obscura) is an Apache-2.0 headless
browser engine in Rust — V8 for script, its own DOM, and (since we last looked)
its own layout and paint stack. CDP-compatible, drop-in for Puppeteer and
Playwright, and it ships an MCP server over JSON-RPC 2.0 on stdio.

Evaluated as a lighter alternative to Chromium for Kastellan's web scraping and
automation path. Current release: **v0.2.2** (2026-09-05).

⚠️ **The original note said Obscura is "a V8-powered DOM scraper (no layout
engine) … not a replacement for the `browser-driver` worker (which needs real
rendering)." That is no longer true.** `crates/obscura-render` is ~68k lines:
`taffy` for layout, `tiny-skia` for rasterization, `cosmic-text`/`swash` for
shaping, `resvg`/`usvg` for SVG. It renders. Whether it renders *well enough*
for a given target is an empirical question, but the categorical objection is
gone.

⚠️ **The workspace `Cargo.toml` still says `version = "0.1.0"` at the v0.2.2
tag.** That is an unbumped internal string, not the release channel. Pin the
git tag, not the manifest version.

## Why the bump

Obscura pins `deno_core = "0.350"` in both `[dependencies]` and
`[build-dependencies]` of `crates/obscura-js/Cargo.toml`. That resolves to
**`v8` crate 137.3.0 — the V8 shipped in Chrome 137**.

⚠️ **The original note described this as "V8 engine 14.5" and framed the fix as
a bump to "V8 14.9". Both numbers were wrong, and the error understated the
gap.** The `v8` crate versions itself by Chrome milestone. Measured against the
v0.2.2 lockfile:

| | `v8` crate | Chrome milestone |
|---|---|---|
| Obscura v0.2.2 as shipped | `137.3.0` | 137 |
| `deno_core 0.405` resolves to | `149.4.0` | 149 |
| Latest `v8` crate (2026-08-20) | `152.2.0` | 152 |

So the engine is roughly **fifteen Chrome milestones behind**, not the four the
original table implied. The three CVEs it listed are real and confirmed, but
they are a sample of the window, not its extent:

| CVE | Notes |
|-----|-------|
| CVE-2026-3910 | V8, exploited in the wild |
| CVE-2026-5281 | V8 zero-day |
| CVE-2026-11645 | OOB read/write in V8, in the wild, CISA KEV, $55k bounty |

CVE-2026-11645 was the *fifth* exploited Chrome zero-day of 2026 (after
CVE-2026-2441, CVE-2026-3909, CVE-2026-3910, CVE-2026-5281), and Google shipped
another actively-exploited V8 zero-day in September 2026. Do not treat the
table above as the closure.

**Obscura's own CI cannot catch this.** It runs `cargo-deny check advisories`,
which reads RUSTSEC — a Rust crate advisory database. V8's own CVEs are not
Rust crate advisories, so they do not appear there. Green CI is not evidence of
a patched engine. (Compare `CLAUDE.md`: *green CI without containment is a
false positive*.)

## Choosing a target version

`deno_core` releases roughly weekly; `0.411.0` (2026-08-27) was latest at the
time of writing. `0.405` is the more conservative target — it still depends on
the `v8` crate directly (`^149.4.0`), whereas `0.411` replaced that with
`deno_v8 ^0.3.0`.

⚠️ **`deno_error` must move in lockstep, at any target version.** Obscura pins
`deno_error = "0.6"`. Measured against crates.io, `deno_core` requires `^0.7.0`
from **0.360 onward** and `=0.7.1` from 0.390. There is no bump above 0.350 that
avoids this. The original note did not mention `deno_error` at all.

⚠️ **`version-strings.patch` is not a patch.** It is a file of shell notes
wearing a diff's filename, and `git apply` will reject it. The CDP version
strings in `crates/obscura-cdp/src/server.rs` must be updated by hand after the
build reports the actual V8 version.

## Status of the bump

**Tested at v0.2.2 on 2026-09-14 (rustc 1.94.1). The bump does not compile.**
The original patch had never been built; it now has been.

⚠️ **`bump-deno-core.patch` is kept as a starting point for a port, not as a
working fix.** Do not treat it as ready to land.

### 0.350 → 0.405 (direct)

Dependency resolution succeeds and moves `v8 137.3.0 -> 149.4.0`. Resolution is
not compilation: `cargo check -p obscura-js` then fails with **111 errors**
(exit 101) in Obscura's own code — 80 in `runtime.rs`, 31 in `ops.rs`, 3 in
`module_loader.rs`.

| Root cause | Evidence | Scale |
|---|---|---|
| **rusty_v8 scope API redesign.** `HandleScope` is now reached through `PinnedRef<HandleScope>` / `ScopeStorage`; `EnteredRuntime::handle_scope`, `is_execution_terminating`, `cancel_terminate_execution`, `exception` no longer resolve. | 47× `E0308`, 9× `E0599` | Pervasive through `runtime.rs` |
| **`deno_error` 0.6 → 0.7.** `JsErrorBox` no longer implements `JsErrorClass`; `From<io::Error>` and `From<ModuleResolutionError>` are gone. | 30× `E0277` | `ops.rs`, `module_loader.rs` |
| **`#[op2]` macro changes.** `async` now requires parenthesised arguments; attribute `global` unknown; several "Invalid v8 type" rejections. | macro-expansion errors | `ops.rs` |
| **`ModuleLoader::load` signature.** 4 parameters in the trait, 5 in the impl. | 1× `E0050` | `module_loader.rs` |
| **`JsError` is boxed.** Error tuples are now `Option<Box<JsError>>`. | within the `E0308`s | `runtime.rs` |

⚠️ **This invalidates the original risk table**, which rated `ops.rs` *"Low —
stable across versions"* (it has 31 errors) and never mentioned `deno_error` or
`module_loader.rs`.

### 0.350 → 0.360 (smallest possible step)

⚠️ **The "just step it" advice in the original note is not viable as written.**
The minimal step fails *before reaching Obscura's code at all*: `deno_core
0.360` pulls `temporal_rs 0.0.11`, which does not compile on rustc 1.94.1
(`unexpected end of macro invocation` in `tzdb.rs:60`). `cargo update -p
temporal_rs` locks 0 packages — the version is pinned within that tree and
cannot be moved. The intermediate `deno_core` releases have their own bit-rot,
so stepping trades one porting problem for a second, unrelated one.

### Conclusion

This is a **porting project, not a patch** — the scope-API work alone is
pervasive surgery across `runtime.rs`. It is not a reasonable thing for
Kastellan to carry as a downstream fork, and it belongs upstream with Obscura,
who own the V8 integration and have the CI to validate it. The practical path is
to raise it with them rather than maintain a patched tree here.

## Relevance to Kastellan

The attraction is not headline benchmarks — it is that
`scripts/workers/microvm/Dockerfile.browser-driver` documents, in its own
comments, a supply chain it cannot pin: ~125 apt packages resolved at build
time, Chromium fetched unpinned from the Playwright CDN, and a dlopen closure
(NSS, fontconfig, SwiftShader) invisible to `ldd` — which is the only reason
that rootfs needs Docker at all. A static Rust binary collapses that back to the
`copy_lib_closure` pattern every other worker rootfs uses.

Other fit notes:

- **Licensing passes.** Apache-2.0, and Obscura's own `deny.toml` allowlist
  (MIT / Apache-2.0 / BSD / ISC / Zlib / MPL-2.0 / BSL-1.0 / Unicode-3.0 /
  CDLA-Permissive-2.0) is AGPL-compatible and CI-enforced, so it should stay
  clean. Note the `[patch.crates-io]` forks of `taffy` and `cosmic-text` under
  `vendor/` bypass cargo-deny's `sources` check and are unreviewed.
- **Cross-platform holds.** No `target_os` conditionals in `crates/**/*.rs`.
  The one platform split is in `obscura-net/Cargo.toml`, which correctly gates
  `wreq`'s `prefix-symbols` feature to Linux/Android and uses plain `wreq`
  elsewhere.
- **IPC already matches.** `obscura-mcp` is 32 tools over line-delimited
  JSON-RPC 2.0 on stdio — that is `kastellan-protocol`.
- **No internal sandbox.** No seccomp/Landlock anywhere in the tree, and V8 runs
  in-process with none of Chrome's multi-process isolation. Under Kastellan the
  worker's bwrap/Firecracker jail still holds the threat-model invariant, but it
  becomes the *only* layer where Chromium gave two. This is the reason to land
  the bump before adoption rather than after.
- **`stealth` adds a second C attack surface.** It swaps rustls for BoringSSL
  and pulls `wreq =6.0.0-rc.29` / `wreq-util =3.0.0-rc.12` — exact-pinned
  release candidates. The exact pins are the right call (a caret range already
  broke their build once, their issue #234), but pre-1.0 rc crates on the
  network path need a human reviewing every upgrade.
- **`panic = "unwind"` is load-bearing.** Obscura's ops wrap bodies in
  `catch_unwind` so a panic degrades to an error instead of unwinding into V8's
  FFI frame and aborting. A custom release profile setting `panic = "abort"`
  would turn every catchable op panic into a hard crash.

## Files

- `bump-deno-core.patch` — the `Cargo.toml` diff (0.350 → 0.405)
- `version-strings.patch` — ⚠️ notes, not an appliable diff (see above)
- `build-and-test.sh` — clone, patch, build, test automation. Note it tracks
  `main`; pin `v0.2.2` if you want the release rather than the tip.
