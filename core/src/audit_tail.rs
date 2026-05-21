//! `tail -f`-style follower for `audit-YYYY-MM-DD.jsonl` files
//! produced by [`crate::audit_mirror`].
//!
//! ## Why a separate module
//!
//! The [`crate::audit_mirror`] producer needs Postgres; the consumer
//! (operator running `hhagent-cli audit tail`) does not — and a key
//! design goal is "the operator can debug a daemon that crashed
//! mid-startup without bringing the cluster up." So the tail path is
//! pure file I/O against the JSONL files, with no DB coupling.
//!
//! ## What this module provides
//!
//! * [`parse_audit_filename`] — pure, extracts the date from
//!   `audit-YYYY-MM-DD.jsonl`. Used by both the CLI driver and the
//!   tests; tested independently.
//! * [`find_audit_files`] — pure(-ish, reads a directory), returns the
//!   audit JSONL files in date-ascending order so a fresh tailer
//!   replays history then switches to live mode.
//! * [`tail_loop`] — async follower that drives the actual streaming.
//!   Pulls new lines from the latest file, polls for a date roll-over,
//!   prints to a writer (stdout in production, a buffer in tests).
//!
//! Polling cadence is 250 ms — fast enough that operators see a new
//! line subsecond after the mirror fsyncs it, slow enough that an
//! idle viewer doesn't busy-poll. inotify/kqueue would shave a few
//! ms but adds a per-OS dep (`notify` crate) for no operator-visible
//! benefit at this scale.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Parse a `audit-YYYY-MM-DD.jsonl` filename into its UTC date.
///
/// Returns `None` for any name that doesn't match the exact shape:
/// the prefix `"audit-"`, a 4-digit year, `'-'`, 2-digit month, `'-'`,
/// 2-digit day, and the suffix `".jsonl"`. The strict shape means
/// stray editor backups (`audit-2026-05-10.jsonl~`) and other files
/// in the state dir are silently ignored.
///
/// Pure: takes a `&str`, returns `Option<time::Date>`.
pub fn parse_audit_filename(name: &str) -> Option<time::Date> {
    let stripped = name.strip_prefix("audit-")?.strip_suffix(".jsonl")?;
    // `stripped` should now be exactly "YYYY-MM-DD" — 10 chars.
    if stripped.len() != 10 {
        return None;
    }
    let bytes = stripped.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i32 = stripped[..4].parse().ok()?;
    let month: u8 = stripped[5..7].parse().ok()?;
    let day: u8 = stripped[8..10].parse().ok()?;
    let month = time::Month::try_from(month).ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

/// List every `audit-*.jsonl` file under `state_dir`, sorted by date
/// ascending (oldest first). Files that don't match the strict
/// filename shape are skipped. A non-existent or unreadable directory
/// yields an empty list — the caller treats "no files yet" as a
/// benign empty stream rather than a hard error.
pub async fn find_audit_files(state_dir: &Path) -> Vec<(time::Date, PathBuf)> {
    let mut entries = match tokio::fs::read_dir(state_dir).await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    // Drive the iterator explicitly so a transient `Err(_)` from
    // `next_entry()` is surfaced (logged) instead of silently
    // truncating the listing. The previous `while let Ok(Some(...))`
    // shorthand conflated `Ok(None)` (legitimate EOF) with `Err(_)`
    // (real iteration error), so a mid-walk failure would yield a
    // partial list and the caller would have no signal that anything
    // went wrong. The actual root cause of issue #101's flake turned
    // out to be a test-harness `tempdir()` collision rather than a
    // mid-iteration `next_entry()` failure, but this defence-in-depth
    // change stands on its own: silent truncation of a directory
    // listing is never the right behaviour for an operator-facing
    // tail follower.
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let name_os = entry.file_name();
                let name = match name_os.to_str() {
                    Some(n) => n,
                    None => continue,
                };
                if let Some(date) = parse_audit_filename(name) {
                    out.push((date, entry.path()));
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::error!(
                    state_dir = %state_dir.display(),
                    err = %e,
                    "find_audit_files: mid-iteration error from next_entry; \
                     returning partial list",
                );
                break;
            }
        }
    }
    out.sort_by_key(|(d, _)| *d);
    out
}

/// Configuration for [`tail_loop`].
pub struct TailConfig {
    /// State directory containing `audit-*.jsonl` files.
    pub state_dir: PathBuf,
    /// When `true`, replay every line from every existing audit file
    /// before switching to follow mode. When `false`, start at EOF of
    /// the latest file (canonical `tail -f` semantics).
    pub from_start: bool,
    /// When `true`, exit after replaying existing content (canonical
    /// `cat` mode). When `false`, follow forever until the process is
    /// signalled.
    pub follow: bool,
}

/// Drive the tail loop until cancellation (SIGINT/Ctrl-C is the
/// expected exit path; the loop has no internal stop condition when
/// `follow=true`).
///
/// Output is unbuffered (each line is `write_all`'d + `flush`'d) so
/// the operator's terminal sees lines as soon as the mirror commits
/// them — buffering would defeat the "operational visibility" goal.
///
/// `out` is generic so tests can capture output into a `Vec<u8>` and
/// production can pass `tokio::io::stdout()`.
pub async fn tail_loop<W>(cfg: TailConfig, mut out: W) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    // (date, path, byte offset). The offset is always derived from the
    // bytes `stream_file_from` actually read — never from an external
    // `metadata().len()` call. A stat-based anchor would race the
    // mirror's writer: bytes appended between read and stat would be
    // skipped (in `from_start` after-the-read), or already-printed
    // bytes would be re-printed (in the follow loop, if the file grew
    // during a read).
    let mut current: Option<(time::Date, PathBuf, u64)> = None;

    if cfg.from_start {
        let files = find_audit_files(&cfg.state_dir).await;
        for (date, path) in &files {
            let bytes = stream_file_from(path, 0, &mut out).await?;
            // Last assignment wins — only the latest-dated file is
            // the live-follow anchor; earlier files are only replayed.
            current = Some((*date, path.clone(), bytes));
        }
    } else if let Some((date, path)) =
        find_audit_files(&cfg.state_dir).await.last().cloned()
    {
        // Live mode: skip existing content, anchor at the end of the
        // latest file. A stat is fine here because we are
        // *intentionally* discarding any bytes between stat and the
        // next loop tick — that's the canonical `tail -f` behaviour
        // (don't replay history). The follow loop still uses
        // bytes-actually-read for its incremental advance, so the
        // skip is bounded to existing-at-attach-time content only.
        let len = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        current = Some((date, path, len));
    }

    if !cfg.follow {
        return Ok(());
    }

    loop {
        // Has a newer-dated file appeared?
        let files = find_audit_files(&cfg.state_dir).await;
        if let Some((latest_date, latest_path)) = files.last() {
            let advance = match &current {
                None => true,
                Some((cur_date, _, _)) => *latest_date > *cur_date,
            };
            if advance {
                // Print everything from the previous file's checkpoint
                // before switching (so a roll-over at exactly midnight
                // doesn't drop the last few lines), then anchor at the
                // start of the new file.
                if let Some((_, path, off)) = &current {
                    let _ = stream_file_from(path, *off, &mut out).await;
                }
                current = Some((*latest_date, latest_path.clone(), 0));
            }
        }

        if let Some((_, path, off)) = current.as_mut() {
            // Stat is only used as a "is there anything new?" gate.
            // The offset itself advances by bytes actually read so
            // a file growing mid-read can't double-print or skip.
            let new_len = match tokio::fs::metadata(&*path).await {
                Ok(m) => m.len(),
                Err(_) => *off, // file vanished briefly; try again next tick
            };
            if new_len > *off {
                let bytes = stream_file_from(path, *off, &mut out).await?;
                *off += bytes;
            }
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Stream `path` from byte offset `from` to EOF, one line per newline,
/// to `out`. Returns the **number of bytes actually streamed** so the
/// caller can advance its offset by exactly that amount — a separate
/// `metadata().len()` would race a writer that's appending to the
/// file during the read.
///
/// We open + seek per call rather than holding an open handle because
/// the mirror writer may have done a `sync_all` + drop-on-rotate; a
/// stale fd would point at a possibly-unlinked inode (matters when an
/// operator runs `logrotate` style cleanup later).
async fn stream_file_from<W>(path: &Path, from: u64, out: &mut W) -> std::io::Result<u64>
where
    W: AsyncWriteExt + Unpin,
{
    use tokio::io::AsyncSeekExt;
    let mut f = tokio::fs::File::open(path).await?;
    if from > 0 {
        f.seek(std::io::SeekFrom::Start(from)).await?;
    }
    let mut reader = BufReader::new(f);
    let mut line = Vec::new();
    let mut total: u64 = 0;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            break;
        }
        out.write_all(&line).await?;
        total += n as u64;
    }
    out.flush().await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_audit_filename_accepts_canonical_shape() {
        let d = parse_audit_filename("audit-2026-05-10.jsonl").unwrap();
        assert_eq!(d.year(), 2026);
        assert_eq!(u8::from(d.month()), 5);
        assert_eq!(d.day(), 10);
    }

    #[test]
    fn parse_audit_filename_rejects_off_shapes() {
        // No prefix.
        assert!(parse_audit_filename("2026-05-10.jsonl").is_none());
        // No suffix.
        assert!(parse_audit_filename("audit-2026-05-10").is_none());
        // Extra suffix.
        assert!(parse_audit_filename("audit-2026-05-10.jsonl.bak").is_none());
        // Wrong digit count.
        assert!(parse_audit_filename("audit-26-5-10.jsonl").is_none());
        assert!(parse_audit_filename("audit-2026-5-10.jsonl").is_none());
        // Non-numeric.
        assert!(parse_audit_filename("audit-XXXX-05-10.jsonl").is_none());
        // Invalid date (Feb 30).
        assert!(parse_audit_filename("audit-2026-02-30.jsonl").is_none());
    }

    #[tokio::test]
    async fn find_audit_files_returns_dates_in_ascending_order() {
        let tmp = tempdir();
        for name in [
            "audit-2026-05-10.jsonl",
            "audit-2026-05-09.jsonl",
            "audit-2026-05-11.jsonl",
            "audit-readme.txt", // ignored
            "junk",             // ignored
        ] {
            tokio::fs::write(tmp.join(name), b"").await.unwrap();
        }
        let files = find_audit_files(&tmp).await;
        let dates: Vec<_> = files.iter().map(|(d, _)| *d).collect();
        assert_eq!(dates.len(), 3);
        assert!(dates.windows(2).all(|w| w[0] <= w[1]), "dates: {dates:?}");
        assert_eq!(dates[0].day(), 9);
        assert_eq!(dates[2].day(), 11);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn find_audit_files_on_missing_dir_returns_empty() {
        let files =
            find_audit_files(Path::new("/no/such/dir/hhagent-tail-test-xyz")).await;
        assert!(files.is_empty());
    }

    /// Small temp-dir helper without a tempfile dep. Combines
    /// pid + system-time-nanos + a process-local atomic counter so
    /// two `#[tokio::test]` cases that race into this function during
    /// the same microsecond still get distinct directories.
    ///
    /// **Why the counter is load-bearing**: the original pid+nanos
    /// suffix was fine on Linux (nanosecond clock resolution is
    /// genuine) but flaked on macOS, where `SystemTime::now()` is
    /// effectively microsecond-resolution. Under tokio's multi-thread
    /// runtime two parallel test cases routinely collide on the
    /// pid+nanos pair, end up sharing a temp dir, and one test's
    /// writes silently overwrite the other's (e.g.
    /// `find_audit_files_returns_dates_in_ascending_order` writes
    /// `audit-2026-05-09.jsonl` with `b""` and
    /// `tail_loop_from_start_replays_then_exits` writes the same
    /// path with `b"a\nb\n"`; whichever wins the race truncates the
    /// other). The atomic counter forces process-local uniqueness
    /// regardless of clock resolution. Closes GitHub issue #101.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "hhagent-audit-tail-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n,
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// from_start mode prints existing lines and (with follow=false)
    /// exits cleanly. The pin here is the byte-for-byte concatenation
    /// of the two test files, in date-ascending order — so a future
    /// refactor that flips the order or drops a trailing newline
    /// trips this test.
    #[tokio::test]
    async fn tail_loop_from_start_replays_then_exits() {
        let tmp = tempdir();
        tokio::fs::write(tmp.join("audit-2026-05-09.jsonl"), b"a\nb\n")
            .await
            .unwrap();
        tokio::fs::write(tmp.join("audit-2026-05-10.jsonl"), b"c\n")
            .await
            .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        tail_loop(
            TailConfig {
                state_dir: tmp.clone(),
                from_start: true,
                follow: false,
            },
            &mut buf,
        )
        .await
        .unwrap();
        assert_eq!(&buf, b"a\nb\nc\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
