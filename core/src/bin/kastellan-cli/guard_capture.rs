//! `guard capture --manifest DIR --out DIR [--record]` — materialise the
//! calibration corpus by driving the **real** `web-fetch` worker.
//!
//! **Why the real worker and not `curl`.** Spec D3. A document reaches
//! the chokepoint through the worker's own extraction and then through
//! [`extract_scannable_text`], which strips keys, flattens leaves
//! alphabetically and truncates at [`SCAN_BYTE_CAP`]. A corpus fetched
//! with `curl` would be scored on text production never sees, and the
//! resulting τ would be fitted against a fiction.
//!
//! **It goes through the existing chokepoint, not around it.**
//! `WorkerCommand::new` is deliberately module-private — its doc calls
//! editing `tool_host` "the reviewable opt-out for the dispatcher
//! chokepoint" — and CLAUDE.md forbids a spawn-unsandboxed escape
//! hatch. So this uses the already-public
//! [`kastellan_core::tool_host::dispatch_with_sink`] and avoids
//! Postgres by passing a **null audit sink**, rather than by avoiding
//! the chokepoint. No new dispatch API is introduced.
//!
//! **One command records and verifies**, so materialising a corpus and
//! capturing it cannot drift apart. Without `--record`, an entry whose
//! hash differs is a hard failure and an entry with no hash at all is
//! refused rather than passed.
//!
//! **`--record` is not a way to skip the check.** It prints the observed
//! hash for the operator to commit, but an entry that already carries a
//! usable hash is still compared and a drifted source is still refused.
//! The reason is the campaign's own shape: the manifest grows to ~90
//! entries and `--record` over the whole directory is the only way to
//! record the new ones, so a flag that also re-pinned the old ones would
//! silently launder every drift it passed over. Recording is for a case
//! never seen before; **re-pinning a changed source has to be a
//! deliberate act**, not a side effect of recording the entries beside
//! it.

use std::path::PathBuf;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

use kastellan_core::cassandra::injection_guard::{extract_scannable_text, SCAN_BYTE_CAP};
use kastellan_core::guard_calibration::corpus::{Label, Provenance};
use kastellan_core::guard_calibration::manifest::{
    load_manifest_from_dir, verify_requirement, ManifestEntry,
};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, AuditSink, WorkerSpec};
use kastellan_core::worker_manifest::{discover_binary, ResolveCtx};
use kastellan_core::workers::web_fetch::web_fetch_entry;

/// SHA-256 of the scannable text, lowercase hex.
///
/// **Over the SCANNABLE text, not the raw response.** That is what
/// production screens, and it is also what makes the hash stable
/// against JSON key ordering — `extract_scannable_text` flattens leaves
/// alphabetically with keys discarded, so a server that reorders its
/// fields does not read as a drifted source.
pub fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Discards every row.
///
/// Capture is an offline corpus-building step, not agent activity:
/// there is no task, no plan and no operator inbox for these rows to
/// belong to, and requiring a live Postgres to fetch a web page would
/// make the corpus harder to reproduce for no gain. The *chokepoint*
/// still runs — only its audit destination is a sink hole.
struct NullSink;

#[async_trait::async_trait]
impl AuditSink for NullSink {
    async fn insert_stored(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, kastellan_db::DbError> {
        Ok(0)
    }
}

/// Was this result the injection placeholder rather than a page?
///
/// **This check is why the corpus cannot be silently corrupted.** On a
/// catalogue `Block`, `post_process::finalize` replaces the worker's
/// result with `injection_blocked_placeholder(..)`. Stored as a corpus
/// case that placeholder is a short, *benign-looking* document that the
/// catalogue does **not** block — so it would enter the adjudicated
/// population and be scored, recording a page as the opposite of what
/// it is.
///
/// Refusing costs nothing *statistically* — a case the catalogue blocks
/// is `excluded_already_blocked` and contributes nothing to τ either
/// way — but it does fail the run, so the entry has to leave the
/// manifest before the campaign can proceed.
#[must_use = "this answers whether the fetched value is a page at all; \
              dropping it stores the withheld placeholder as a corpus case"]
fn is_injection_placeholder(v: &serde_json::Value) -> bool {
    v.get(kastellan_core::tool_host::INJECTION_BLOCKED_KEY) == Some(&serde_json::Value::Bool(true))
}

/// What one manifest entry produced.
enum Outcome {
    Materialised {
        text: String,
        sha256: String,
        truncated: bool,
        /// The manifest already carried this exact hash. Distinguishes
        /// `RECORD-SAME` from `RECORD-NEW` so an operator reading ~90
        /// lines of output can see which entries are new rather than
        /// diffing every hash by eye.
        already_recorded: bool,
    },
    Refused(String),
}

pub fn run(args: &[String]) -> ExitCode {
    let mut manifest_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut record = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                i += 1;
                match args.get(i) {
                    Some(p) => manifest_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--manifest requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(p) => out_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--out requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--record" => record = true,
            other => {
                eprintln!("{USAGE}");
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let (Some(manifest_dir), Some(out_dir)) = (manifest_dir, out_dir) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let entries = match load_manifest_from_dir(&manifest_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // In verify mode every entry must already carry a usable hash.
    // Checked for the WHOLE manifest before any fetch, so a manifest-wide
    // omission is reported without spending a single network round trip
    // — and so a partial materialisation cannot leave the out dir in a
    // state that looks complete.
    if !record {
        let refusals: Vec<String> = entries.iter().filter_map(verify_requirement).collect();
        if !refusals.is_empty() {
            for r in &refusals {
                eprintln!("REFUSED {r}");
            }
            eprintln!("\n{} entries cannot be verified; nothing fetched.", refusals.len());
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let mut failures = 0usize;
    for entry in &entries {
        match capture_one(entry, record) {
            Ok(Outcome::Refused(why)) => {
                eprintln!("REFUSED {}: {why}", entry.id);
                discard_stale(&out_dir, entry);
                failures += 1;
            }
            Ok(Outcome::Materialised { text, sha256, truncated, already_recorded }) => {
                // The truncation note is on BOTH lines. A verify run
                // that stayed silent about it could not tell the
                // operator which cases exercised the cap, which is a
                // stratum measurement 3 owes ≥ 8 of.
                let capped = if truncated { ", truncated at the cap" } else { "" };
                // Written BEFORE the line is printed. The `RECORD-*`
                // line is what an operator pastes into the manifest, and
                // printing it for a case whose file failed to write
                // would pin a hash for a case the corpus does not
                // contain -- a later verify run then passes over a
                // silently shorter population.
                if let Err(e) = write_case(&out_dir, entry, &text) {
                    eprintln!("WRITE-FAILED {}: {e}", entry.id);
                    discard_stale(&out_dir, entry);
                    failures += 1;
                } else if record {
                    let verb = if already_recorded { "RECORD-SAME" } else { "RECORD-NEW" };
                    println!("{verb} {} {sha256} ({} bytes{capped})", entry.id, text.len());
                } else {
                    println!("OK {} ({} bytes{capped})", entry.id, text.len());
                }
            }
            Err(e) => {
                eprintln!("FETCH-FAILED {}: {e}", entry.id);
                discard_stale(&out_dir, entry);
                failures += 1;
            }
        }
    }

    report_orphans(&out_dir, &entries);

    if failures > 0 {
        eprintln!("\n{failures} of {} entries failed.", entries.len());
        return ExitCode::FAILURE;
    }
    println!(
        "\n{} entries materialised into {}",
        entries.len(),
        out_dir.display()
    );
    ExitCode::SUCCESS
}

const USAGE: &str = "usage: kastellan-cli guard capture --manifest DIR --out DIR [--record]";

/// Fetch one entry through the real sandboxed worker and hash what the
/// chokepoint saw.
fn capture_one(entry: &ManifestEntry, record: bool) -> Result<Outcome, String> {
    let fetched = fetch_through_worker(&entry.source)?;
    if is_injection_placeholder(&fetched) {
        return Ok(Outcome::Refused(format!(
            "the catalogue already blocks {}, so dispatch returned the withheld \
             placeholder rather than the page. Storing it would record a \
             benign-looking document in place of the real one -- and a case the \
             catalogue blocks is excluded from the fit anyway, so nothing is lost \
             by dropping it",
            entry.source
        )));
    }
    // **An error page is a document too, and it hashes just fine.**
    // `fetch::drive` returns any non-3xx response as `Ok` and the worker
    // passes `status` through untouched, so a vanished Wayback snapshot
    // yields a 404 body that `extract_scannable_text` reduces to text
    // like any other. Stored, it becomes a corpus case wearing the label
    // and notes of the page it replaced -- the same corruption
    // `is_injection_placeholder` refuses one door along.
    //
    // Nothing downstream can catch it: `walk` emits string leaves only,
    // so the status is in neither the stored text nor the sha256, and
    // under `--record` the error page's hash would be pinned *as the
    // truth*. The check has to be here or nowhere.
    //
    // After the placeholder check, not before it: a catalogue Block
    // substitutes a value that carries no `status` at all, and reporting
    // that as a shape change would name the wrong cause.
    match fetched.get("status").and_then(serde_json::Value::as_u64) {
        Some(s) if (200..300).contains(&s) => {}
        Some(s) => {
            return Ok(Outcome::Refused(format!(
                "HTTP {s} from {}. An error page's body is still a document and \
                 would be stored under this case's label; re-point the entry at a \
                 snapshot that resolves",
                entry.source
            )))
        }
        None => {
            return Err(format!(
                "web-fetch returned no `status` for {}, so this capture cannot be \
                 shown to be a successful fetch. The worker's result shape has \
                 changed; fix that before trusting anything captured with it",
                entry.source
            ))
        }
    }

    let (text, truncated) = extract_scannable_text(&fetched, SCAN_BYTE_CAP);
    let observed = sha256_hex(&text);

    // `Ok` exactly when the entry carries a usable hash, so one match
    // answers both modes' questions: verify mode asks "does it still
    // match?", record mode asks "is there anything here to contradict?"
    match entry.verified_sha256() {
        Ok(expected) if observed != expected => Ok(Outcome::Refused(if record {
            format!(
                "manifest already records {expected}, but the source now yields \
                 {observed}. --record will not silently re-pin a drifted source: \
                 investigate, then clear this entry's sha256 deliberately if the \
                 new bytes are the ones you want"
            )
        } else {
            format!(
                "manifest {expected}, observed {observed}. The source has drifted; \
                 investigate before trusting any tau fitted against it. NOTE that a \
                 change to the web-fetch extractor or to extract_scannable_text \
                 moves every recorded hash too, so rule that out before concluding \
                 the source is what moved"
            )
        })),
        Ok(_) => Ok(Outcome::Materialised {
            text,
            sha256: observed,
            truncated,
            already_recorded: true,
        }),
        // No usable hash: exactly what `--record` is for. Verify mode
        // never reaches this arm -- the whole manifest was checked before
        // the first fetch -- but it is a returned error rather than an
        // `expect` so a future caller that skips the pre-check gets a
        // diagnosis instead of a panic on a security control's path.
        Err(why) if !record => Err(why),
        Err(_) => Ok(Outcome::Materialised {
            text,
            sha256: observed,
            truncated,
            already_recorded: false,
        }),
    }
}

/// Remove a previous run's file for an entry that did **not** succeed now.
///
/// **A failed entry must not leave last run's text behind.** Nothing
/// downstream reconciles this directory against the manifest --
/// `load_corpus_from_dir` accepts whatever `*.json` it finds -- so a
/// stale case for a REFUSED or FETCH-FAILED entry is scored as though it
/// had been captured and verified *this* run, silently altering the
/// population τ is fitted over. The exit code says the run failed; the
/// directory has to say so too, because the two are read by different
/// people at different times.
fn discard_stale(out_dir: &std::path::Path, entry: &ManifestEntry) {
    let path = out_dir.join(format!("{}.json", entry.id));
    match std::fs::remove_file(&path) {
        Ok(()) => eprintln!("  discarded the previous {}", path.display()),
        // Nothing to discard is the ordinary case on a first run.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "  WARNING: could not discard the previous {}: {e}. It is stale and \
             WILL be scored if you calibrate against this directory.",
            path.display()
        ),
    }
}

/// Name every `*.json` in the out dir that no manifest entry produced.
///
/// **A warning, not a failure, and the distinction is the runbook's.**
/// The campaign deliberately calibrates over a directory holding the
/// materialised cases *and* the committed seeded corpus, so "files this
/// manifest did not write" is a documented steady state and refusing it
/// would refuse the intended workflow.
///
/// What is not intended is a case left behind by an entry since retired
/// from the manifest. It is scored, it moves τ, and nothing else would
/// ever mention it: `load_corpus_from_dir` rejects an *empty* directory
/// but is blind to an over-full one, and the dir is gitignored so
/// `git status` will not show it either.
fn report_orphans(out_dir: &std::path::Path, entries: &[ManifestEntry]) {
    let known: std::collections::BTreeSet<&str> =
        entries.iter().map(|e| e.id.as_str()).collect();
    let dir = match std::fs::read_dir(out_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "WARNING: cannot list {} ({e}), so files this manifest did not \
                 write could not be reported.",
                out_dir.display()
            );
            return;
        }
    };
    let mut orphans: Vec<String> = Vec::new();
    for f in dir {
        let path = match f {
            Ok(f) => f.path(),
            Err(e) => {
                eprintln!("WARNING: skipped an unreadable directory entry: {e}");
                continue;
            }
        };
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) if !known.contains(stem) => orphans.push(stem.to_string()),
            _ => {}
        }
    }
    if orphans.is_empty() {
        return;
    }
    orphans.sort();
    eprintln!(
        "\nNOTE: {} file(s) in {} were not written by this manifest and will \
         still be scored if you calibrate against this directory:",
        orphans.len(),
        out_dir.display()
    );
    // Capped, and the cap SAYS SO. The runbook's own flow copies the
    // seeded corpus into this directory before calibrating, so a re-run
    // legitimately finds a couple of dozen; listing ~90 would bury the
    // failures above it, and truncating silently would read as "that was
    // all of them".
    const SHOWN: usize = 10;
    for o in orphans.iter().take(SHOWN) {
        eprintln!("  ORPHAN {o}.json");
    }
    if orphans.len() > SHOWN {
        eprintln!("  ... and {} more, not listed", orphans.len() - SHOWN);
    }
}

fn write_case(
    out_dir: &std::path::Path,
    entry: &ManifestEntry,
    text: &str,
) -> Result<(), String> {
    // `Label` derives `Deserialize` only, so the wire spelling is
    // written out here rather than derived. That is the right way round:
    // this is the format the corpus loader must accept, so a mismatch is
    // a load error rather than a silent rename following a Rust
    // identifier.
    let label = match entry.label {
        Label::Attack => "attack",
        Label::Benign => "benign",
    };
    // Provenance, by contrast, IS derived -- from the manifest entry's
    // own field, through `Provenance`'s wire spelling. A literal here
    // would stamp every case `captured` even if `ManifestProvenance`
    // gained a second variant, putting cases in the wrong stratum, which
    // is precisely what makes `BudgetScope` count the wrong benigns and
    // tau drift below D7's criterion.
    let provenance = Provenance::from(entry.provenance).as_str();
    let case = serde_json::json!({
        "id": entry.id,
        "label": label,
        "provenance": provenance,
        "text": text,
        "notes": entry.notes,
    });
    // `id` cannot escape `out_dir`: `load_manifest_from_dir` pins
    // `id == <filename stem>`, and a `read_dir` stem carries no
    // separator and is never absolute.
    let path = out_dir.join(format!("{}.json", entry.id));
    let body = serde_json::to_vec_pretty(&case).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))
}

/// The single host this entry's fetch may reach, derived from its own
/// `source`.
///
/// **Refuses rather than mis-derives.** The returned string becomes the
/// worker's entire network allowlist, so every shape that would make the
/// token mean something other than "the host in this URL" is refused:
///
/// * **userinfo** (`https://a@b/x`) — the token would be `a@b`, which is
///   not the host the worker connects to;
/// * **an explicit port** — [`web_fetch_entry`] appends `:443`, so
///   `h:8443` becomes the meaningless `h:8443:443`, failing closed with
///   a confusing message instead of an accurate one;
/// * **a leading dot** (`https://.example.com/x`) — the worker's own
///   allowlist parser reads a leading dot as a **subdomain wildcard**, so
///   a typo would widen the grant from one host to a whole domain. This
///   is the only shape here that fails *open*, which is why it is
///   checked rather than left to fail closed on its own;
/// * **an IP literal in a denied range** — see below;
/// * an empty authority.
///
/// **The denied-range check is here because nothing downstream does it.**
/// [`web_fetch_entry`] leaves `proxy_uds` unset, so this is the direct
/// `Net::Allowlist` path and the egress proxy's SSRF check never runs; a
/// `source` of `https://169.254.169.254/latest/meta-data/` would
/// otherwise authorise itself. This catches **IP literals only** — a
/// *hostname* resolving into a denied range still needs the proxy, which
/// is [#594](https://github.com/hherb/kastellan/issues/594).
fn allowlist_host(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("{url:?} is not an https URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    if authority.contains('@') {
        return Err(format!(
            "{url:?} carries userinfo, so the allowlist token would not be the \
             host the worker connects to"
        ));
    }
    let host = authority.to_ascii_lowercase();
    // A colon inside brackets is part of an IPv6 literal, not a port.
    let has_port = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').is_some_and(|(_, tail)| !tail.is_empty()),
        None => host.contains(':'),
    };
    if has_port {
        return Err(format!(
            "{url:?} names an explicit port. The grant is built as `host:443`, so \
             this would read `{host}:443` and match nothing"
        ));
    }
    if host.starts_with('.') {
        return Err(format!(
            "{url:?} has a leading dot in its host, which the worker's allowlist \
             reads as a SUBDOMAIN WILDCARD -- it would widen the grant from one \
             host to a whole domain"
        ));
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if kastellan_net_classify::is_denied_range(ip) {
            return Err(format!(
                "{url:?} names {ip}, which is in a denied range. This path has no \
                 egress proxy, so nothing downstream would refuse it"
            ));
        }
    }
    Ok(host)
}

/// Fetch one URL through the real sandboxed `web-fetch` worker.
///
/// **The egress grant comes from this entry's own `source`, not from the
/// database.** [`web_fetch_entry`] takes the allowlist as an argument, so
/// capture is self-provisioning and minimally scoped: each fetch permits
/// exactly the host it is about to contact, mapped to `host:443`, and
/// nothing else. **No `tool_allowlists` row and no daemon restart are
/// needed** — proved by removing the row and re-running for
/// byte-identical hashes. (This comment used to claim the opposite,
/// reasoning from the *daemon's* dispatch path where every deployed
/// web-fetch attempt has died on `-32001: host … not on allowlist`. That
/// applies to the daemon, not to this command.)
///
/// What it does require is a discoverable worker binary
/// (`current_exe()`-relative).
fn fetch_through_worker(url: &str) -> Result<serde_json::Value, String> {
    let host = allowlist_host(url)?;

    // The same `current_exe()`-relative discovery the daemon runs, driven
    // through the identical `ResolveCtx` seam (cf. `broker::config`), so a
    // capture run and a live dispatch cannot resolve different binaries.
    //
    // Reported rather than folded into `None`: as `.ok()` a `current_exe`
    // failure surfaced as "not found next to this binary; build the
    // workspace", sending the operator to rebuild a workspace that is fine.
    let exe_dir = match std::env::current_exe() {
        Ok(p) => p.parent().map(std::path::Path::to_path_buf),
        Err(e) => {
            return Err(format!(
                "cannot locate this binary ({e}), so the worker cannot be \
                 discovered next to it"
            ))
        }
    };
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &std::path::Path| p.exists();
    let is_dir = |p: &std::path::Path| p.is_dir();
    let allowlist = |_t: &str| Vec::new();
    let canonicalize = |p: &std::path::Path| std::fs::canonicalize(p).ok();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &is_dir,
        exe_dir: exe_dir.as_deref(),
        canonicalize: &canonicalize,
        allowlist: &allowlist,
    };
    let worker_path =
        discover_binary(&ctx, "KASTELLAN_WEB_FETCH_BIN", "kastellan-worker-web-fetch")
            .ok_or_else(|| {
                "kastellan-worker-web-fetch not found next to this binary; build the \
                 workspace or run `kastellan-cli install`"
                    .to_string()
            })?;
    let entry = web_fetch_entry(worker_path.clone(), &[host]);
    let backend = kastellan_sandbox::default_backend();
    let worker_str = worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &worker_str,
        args: &[],
        // The timeout declared beside the policy this spec already uses.
        // Dropped, `spawn_worker` builds no watchdog AT ALL, so a stalled
        // origin hangs an 85-entry campaign with no diagnostic and no
        // exit code.
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let result = rt.block_on(async {
        dispatch_with_sink(
            &NullSink,
            &Vault::new(),
            // No guard tier: `guard capture` records what the DETERMINISTIC
            // chokepoint saw, and the corpus it builds is what the model tier
            // is later calibrated against. Running the model here would score
            // the corpus with the thing the corpus exists to measure.
            None,
            &mut worker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({ "url": url }),
        )
        .await
        .map_err(|e| e.to_string())
    });
    if let Err(e) = worker.close() {
        // Not fatal -- the dispatch result is already in hand, so the
        // captured text is not at risk. Said out loud because a sandbox
        // that failed to reap is otherwise invisible across ~90 fetches.
        eprintln!("  warning: could not reap the web-fetch worker for {url}: {e}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash is over text, so it is stable against anything
    /// `extract_scannable_text` normalises away.
    #[test]
    fn sha256_hex_is_lowercase_and_64_chars() {
        let h = sha256_hex("hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(h, sha256_hex("hell0"), "must actually depend on the input");
    }

    /// The placeholder detector must fire on the real shape and not on
    /// an ordinary page that happens to mention the words.
    #[test]
    fn the_placeholder_is_detected_by_its_flag_not_its_prose() {
        let placeholder = serde_json::json!({
            "injection_blocked": true,
            "note": "[tool output withheld: failed injection screen]",
            "score": 0.9,
            "reason_codes": ["instruction_override"],
        });
        assert!(is_injection_placeholder(&placeholder));

        // A page ABOUT injection blocking is not a placeholder. Under
        // D4 that is a benign captured case and must materialise.
        let article = serde_json::json!({
            "title": "How injection_blocked placeholders work",
            "text": "When the screen fires, the tool output is withheld.",
        });
        assert!(!is_injection_placeholder(&article));

        // A page that merely carries the key with a non-true value.
        let odd = serde_json::json!({ "injection_blocked": "true" });
        assert!(
            !is_injection_placeholder(&odd),
            "a string is not the boolean the chokepoint writes"
        );
    }

    /// The detector is coupled to the PRODUCER, not to a copy of its
    /// shape.
    ///
    /// The test above asserts a fact about a JSON literal written here.
    /// Rename the `injection_blocked` key in
    /// `injection_blocked_placeholder` and that test stays green while
    /// the detector goes permanently blind -- which is exactly the
    /// silent corpus corruption its doc says it prevents.
    #[test]
    fn the_placeholder_shape_comes_from_the_producer_not_a_copy() {
        let real = kastellan_core::tool_host::injection_blocked_placeholder(
            0.9,
            &["instruction_override"],
        );
        assert!(
            is_injection_placeholder(&real),
            "the chokepoint's own placeholder must be detected: {real}"
        );
    }

    fn entry(id: &str, label: &str, source: &str) -> ManifestEntry {
        // Built through `Deserialize` rather than a struct literal:
        // `sha256` is private so that `verified_sha256` really is the
        // only accessor, and this is also the only way an entry is ever
        // born in production.
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","label":"{label}","provenance":"captured",
                 "source":"{source}","notes":"a note"}}"#
        ))
        .expect("fixture parses")
    }

    /// The writer and the reader agree on the wire shape.
    ///
    /// **Nothing else links them.** `write_case` hand-builds the JSON,
    /// `CorpusCase` is `deny_unknown_fields` and `Deserialize`-only, and
    /// a third copy lives in `guard_calibrate_cli_e2e`. Swapping the two
    /// `Label` arms mislabels every materialised case and inverts TP and
    /// FP corpus-wide; before this test, `cargo test --workspace` did not
    /// notice.
    #[test]
    fn a_written_case_loads_back_with_its_label_and_provenance_intact() {
        use kastellan_core::guard_calibration::corpus::load_corpus_from_dir;

        let d = tempfile::tempdir().expect("tempdir");
        write_case(d.path(), &entry("cap-001-a", "attack", "https://e.example/a"), "aaa")
            .expect("write attack");
        write_case(d.path(), &entry("cap-002-b", "benign", "https://e.example/b"), "bbb")
            .expect("write benign");

        let back = load_corpus_from_dir(d.path()).expect("the corpus loader accepts it");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, "cap-001-a");
        assert_eq!(back[0].label, Label::Attack, "the label must survive the round trip");
        assert_eq!(back[1].label, Label::Benign);
        for c in &back {
            assert_eq!(
                c.provenance,
                Provenance::Captured,
                "a materialised case is captured by construction"
            );
        }
    }

    /// The derived host IS the worker's whole network allowlist, so
    /// every shape that would make it mean something else is refused.
    #[test]
    fn allowlist_host_refuses_every_shape_that_would_mis_derive_the_grant() {
        assert_eq!(allowlist_host("https://example.com/a/b").unwrap(), "example.com");
        assert_eq!(allowlist_host("https://EXAMPLE.com/a").unwrap(), "example.com");
        // A query or fragment before the first slash is still authority-terminating.
        assert_eq!(allowlist_host("https://example.com?q=1").unwrap(), "example.com");
        assert_eq!(allowlist_host("https://example.com#f").unwrap(), "example.com");
        assert_eq!(allowlist_host("https://example.com").unwrap(), "example.com");

        for (url, needle) in [
            // The one that fails OPEN without this check: a leading dot
            // is a subdomain wildcard to the worker's allowlist parser.
            ("https://.example.com/x", "SUBDOMAIN WILDCARD"),
            ("https://user@example.com/x", "userinfo"),
            ("https://example.com:8443/x", "explicit port"),
            ("https:///x", "no host"),
            ("http://example.com/x", "not an https URL"),
            // Denied ranges: nothing downstream refuses these, because
            // this path has no egress proxy.
            ("https://127.0.0.1/x", "denied range"),
            ("https://169.254.169.254/latest/meta-data/", "denied range"),
            ("https://[::1]/x", "denied range"),
            ("https://10.0.0.1/x", "denied range"),
        ] {
            let err = allowlist_host(url).expect_err(&format!("{url} must be refused"));
            assert!(
                err.contains(needle),
                "{url}: expected the refusal to say {needle:?}, got {err:?}"
            );
        }

        // A public literal is fine -- the check is the RANGE, not the
        // fact of being an IP.
        assert_eq!(allowlist_host("https://93.184.216.34/x").unwrap(), "93.184.216.34");
    }
}
