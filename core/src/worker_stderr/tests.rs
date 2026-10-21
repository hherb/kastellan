use super::*;
use std::io::Cursor;

#[test]
fn tail_retains_bounded_lines_oldest_evicted() {
    let tail = StderrTail::new(2);
    tail.push("one".into());
    tail.push("two".into());
    tail.push("three".into());
    assert_eq!(tail.snapshot(), vec!["two".to_string(), "three".to_string()]);
}

#[test]
fn tail_zero_cap_retains_nothing() {
    let tail = StderrTail::new(0);
    tail.push("x".into());
    assert!(tail.snapshot().is_empty());
}

#[test]
fn drain_reader_populates_tail_with_complete_and_partial_lines() {
    let tail = StderrTail::new(10);
    // Two newline-terminated lines + a trailing partial line (no `\n`).
    let data = b"first line\nsecond line\npartial".to_vec();
    assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
    assert_eq!(
        tail.snapshot(),
        vec!["first line".to_string(), "second line".to_string(), "partial".to_string()]
    );
}

/// A reader that yields its bytes and then fails, standing in for the
/// guest fd that returns `EIO` when a micro-VM or container is torn down —
/// [#747](https://github.com/hherb/kastellan/issues/747)'s motivating case.
struct FailsAfterData {
    data: Cursor<Vec<u8>>,
    /// Errors yielded before any real read, to exercise the `EINTR` arm.
    interrupts: usize,
    /// The errno the final read fails with.
    errno: i32,
}

impl Read for FailsAfterData {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.interrupts > 0 {
            self.interrupts -= 1;
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "EINTR"));
        }
        match self.data.read(buf)? {
            // Where a real pipe would report EOF, report a hard failure
            // instead: the drain stops, but NOT because the worker is done.
            0 => Err(std::io::Error::from_raw_os_error(self.errno)),
            n => Ok(n),
        }
    }
}

#[test]
fn a_read_error_ends_the_drain_as_failed_not_as_eof() {
    // #747's core. Before this, `drain_reader` returned nothing and the
    // spawner marked the tail drained unconditionally, so a pipe that blew
    // up read as "the worker closed its stderr" — and an empty tail then
    // earned the "wrote NOTHING — suspect a kill" diagnosis for a worker
    // that may have explained itself perfectly into a pipe we lost.
    let tail = StderrTail::new(8);
    let reader = FailsAfterData {
        data: Cursor::new(b"worker booting\n".to_vec()),
        interrupts: 0,
        errno: libc::EIO,
    };
    assert_eq!(
        drain_reader(0, reader, Some(&tail)),
        DrainEnd::ReadError(ReadFault::os(libc::EIO)),
        "a drain stopped by a read error must say so — and say WHICH (#752); reporting EOF \
         is what lets a renderer diagnose the worker for a fault in our own transport"
    );
    assert_eq!(
        tail.snapshot(),
        vec!["worker booting".to_string()],
        "POSITIVE CONTROL: whatever DID arrive before the error is still retained — a \
         failed drain is not a reason to discard the lines we got"
    );
    // A second errno, so the fault must come from the read and not from a
    // constant that happens to match the motivating case.
    let ebadf = FailsAfterData { data: Cursor::new(vec![]), interrupts: 0, errno: libc::EBADF };
    assert_eq!(drain_reader(0, ebadf, None), DrainEnd::ReadError(ReadFault::os(libc::EBADF)));
}

#[test]
fn an_interrupted_read_is_retried_rather_than_ending_the_drain() {
    // The `EINTR` arm is `continue`, not `break`, and the distinction is
    // easy to lose while editing the loop that #747 changed. A signal
    // arriving mid-read must not truncate a worker's explanation.
    let tail = StderrTail::new(8);
    let reader = FailsAfterData {
        data: Cursor::new(b"survived the signal\n".to_vec()),
        interrupts: 3,
        errno: libc::EIO,
    };
    assert_eq!(drain_reader(0, reader, Some(&tail)), DrainEnd::ReadError(ReadFault::os(libc::EIO)));
    assert_eq!(
        tail.snapshot(),
        vec!["survived the signal".to_string()],
        "three EINTRs before the first real read must be retried, not treated as the end \
         of the stream"
    );
}

#[test]
fn the_drain_thread_records_a_read_error_on_the_tail() {
    // The seam between `drain_reader`'s verdict and the tail every
    // renderer reads. In production this runs on a detached thread over a
    // `ChildStderr`, which is exactly why it is a separate generic
    // function: hard-coding `DrainEnd::Eof` here reintroduces #747 in full
    // and no other test in the tree can see it.
    let tail = StderrTail::new(8);
    drain_and_mark(
        0,
        FailsAfterData { data: Cursor::new(b"last thing\n".to_vec()), interrupts: 0, errno: libc::EIO },
        &tail,
    );
    assert_eq!(
        tail.wait_for_drain(Duration::from_millis(0)),
        Some(DrainEnd::ReadError(ReadFault::os(libc::EIO))),
        "the drain thread must record WHY it stopped, not merely that it stopped"
    );

    // POSITIVE CONTROL: the same helper over a clean reader records EOF,
    // so the assertion above cannot pass by the helper always reporting a
    // failure.
    let clean = StderrTail::new(8);
    drain_and_mark(0, Cursor::new(b"all done\n".to_vec()), &clean);
    assert_eq!(clean.wait_for_drain(Duration::from_millis(0)), Some(DrainEnd::Eof));
}

#[test]
fn a_failed_drain_is_collected_as_drain_failed_not_as_complete() {
    // The end-to-end of #747 at the seam a unit test can reach: the
    // spawner stores whatever `drain_reader` returned, and
    // `collect_tail_after_drain` must carry it into the rendered state
    // rather than flattening it to "complete".
    let tail = StderrTail::new(8);
    tail.mark_drained(DrainEnd::ReadError(ReadFault::os(libc::EIO)));
    let collected = collect_tail_after_drain(&tail);
    assert_eq!(
        collected.state(),
        TailState::DrainFailedSilent(ReadFault::os(libc::EIO)),
        "an empty tail whose drain FAILED must never reach a renderer as KnownSilent — \
         that is the one state licensed to print a diagnosis"
    );
}

#[test]
fn a_drain_end_is_recorded_once_and_a_second_mark_cannot_overwrite_it() {
    // #752: a drain ends once; a second mark must not turn a read failure
    // into EOF (the #747 lie, through the back door).
    let tail = StderrTail::new(4);
    let failed = DrainEnd::ReadError(ReadFault::os(libc::EIO));
    tail.mark_drained(failed);
    tail.mark_drained(DrainEnd::Eof);
    assert_eq!(
        tail.wait_for_drain(Duration::ZERO),
        Some(failed),
        "the FIRST recorded outcome must stand; a later mark must not overwrite it"
    );
    // The reverse order, so "a read error always wins" cannot pass for
    // write-once.
    let clean = StderrTail::new(4);
    clean.mark_drained(DrainEnd::Eof);
    clean.mark_drained(failed);
    assert_eq!(clean.wait_for_drain(Duration::ZERO), Some(DrainEnd::Eof), "write-once, not prefer-error");
}

#[test]
fn drain_reader_skips_blank_lines_and_survives_non_utf8() {
    let tail = StderrTail::new(10);
    // A blank line between two real ones, plus a stray non-UTF-8 byte (0xff)
    // that must not halt the drain.
    let mut data = b"alpha\n\nbeta".to_vec();
    data.push(0xff);
    assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
    let snap = tail.snapshot();
    assert_eq!(snap[0], "alpha");
    // The blank line is skipped; the trailing chunk (beta + replacement char)
    // is retained as one line.
    assert_eq!(snap.len(), 2, "blank line not retained: {snap:?}");
    assert!(snap[1].starts_with("beta"), "got {:?}", snap[1]);
}

#[test]
fn drain_reader_bounds_newline_free_carry() {
    // A worker streaming to stderr without ever emitting a `\n` must not grow
    // the carry buffer without limit; the drain flushes it as synthetic lines
    // once it crosses MAX_CARRY_BYTES, so the tail captures the output and
    // memory stays bounded (#350 review).
    let tail = StderrTail::new(DEFAULT_TAIL_LINES);
    let data = vec![b'x'; MAX_CARRY_BYTES * 3 + 7]; // no newline anywhere
    assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
    let snap = tail.snapshot();
    assert!(!snap.is_empty(), "newline-free output should still be captured");
    // No retained line exceeds the cap by more than a single read chunk's worth.
    for line in &snap {
        assert!(
            line.len() <= MAX_CARRY_BYTES + 8192,
            "carry line not bounded: {} bytes",
            line.len()
        );
    }
}

#[test]
fn drain_reader_without_tail_does_not_panic() {
    // The no-tail path (tool_host's use) just drains; it must run cleanly.
    assert_eq!(drain_reader(0, Cursor::new(b"noisy\nworker\n".to_vec()), None), DrainEnd::Eof);
}

#[test]
fn wait_for_drain_returns_once_the_drainer_finishes() {
    let tail = StderrTail::new(4);
    assert!(
        tail.wait_for_drain(Duration::from_millis(10)).is_none(),
        "an un-drained tail must time out rather than claim completion"
    );
    tail.mark_drained(DrainEnd::Eof);
    assert!(
        tail.wait_for_drain(Duration::from_millis(10)) == Some(DrainEnd::Eof),
        "a tail drained to EOF must be observed as drained, and as having reached EOF"
    );
}

#[test]
fn drain_then_mark_makes_the_tail_observably_complete() {
    // The whole point of the flag: a caller reacting to a worker death must
    // be able to WAIT for the explanation instead of racing it. This is the
    // exact sequence `spawn_drain_with_tail`'s thread runs, driven from a
    // Cursor so it needs no real child process.
    let tail = StderrTail::new(4);
    let t = tail.clone();
    let h = std::thread::spawn(move || {
        assert_eq!(drain_reader(0, Cursor::new(b"boom\n".to_vec()), Some(&t)), DrainEnd::Eof);
        t.mark_drained(DrainEnd::Eof);
    });
    assert!(
        tail.wait_for_drain(Duration::from_secs(5)) == Some(DrainEnd::Eof),
        "the drain must be observed as complete"
    );
    h.join().unwrap();
    assert_eq!(tail.snapshot(), vec!["boom".to_string()]);
}

#[test]
fn drain_reader_neutralises_terminal_controls_before_they_reach_the_log() {
    // A compromised worker is in scope, and this stream now reaches the
    // daemon log at WARN via `format_worker_failure_report` — i.e. visible in
    // an operator's terminal by default rather than only under `debug`.
    // An ESC here would be an ANSI sequence executing in that terminal.
    let tail = StderrTail::new(10);
    assert_eq!(
        drain_reader(
            0,
            Cursor::new("red\u{1b}[31m and 8-bit \u{9b}31m\nsecond\n".as_bytes().to_vec()),
            Some(&tail),
        ),
        DrainEnd::Eof
    );
    let got = tail.snapshot();
    // TWO lines, not one: `\n` is itself in the neutralised class, so
    // neutralising before the split would collapse the whole stream into a
    // single line — which is how the first version of this broke.
    assert_eq!(got.len(), 2, "the line split must survive neutralisation: {got:?}");
    assert_eq!(got[1], "second", "{got:?}");
    assert!(
        !got[0].contains('\u{1b}') && !got[0].contains('\u{9b}'),
        "both the 7-bit ESC and the 8-bit CSI must be neutralised: {:?}",
        got[0]
    );
    assert!(
        got[0].contains("red") && got[0].contains("[31m"),
        "the surrounding text must survive — a log line is evidence: {:?}",
        got[0]
    );
}
#[test]
fn collecting_a_worker_tail_waits_for_the_drainer_instead_of_racing_it() {
    // The property #735 restored: a worker that has just closed its pipes
    // has usually NOT been drained yet, so snapshotting immediately yields
    // an empty ring and `format_death_report` renders "no stderr captured"
    // for a worker that explained itself a millisecond later. That is the
    // contentless line #730's whole point was to avoid.
    //
    // The drainer here lands its line AFTER this thread has already called
    // `collect_tail_after_drain`, which is exactly the race in production. Deleting
    // the `wait_for_drain` makes this return empty and the test fails.
    let tail = StderrTail::new(8);
    let writer = tail.clone();
    let drain = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let end = drain_reader(
            0,
            std::io::Cursor::new(b"matrix sync failed, refusing to continue\n".to_vec()),
            Some(&writer),
        );
        writer.mark_drained(end);
    });

    let collected = collect_tail_after_drain(&tail);
    drain.join().expect("the fixture's drain thread");

    // Asserts the WHOLE `CapturedTail`, flag included: the drainer finished
    // well inside the 250 ms cap, so the correct verdict is `complete`.
    // Before #732 this function returned only the lines, and a mutant that
    // reported every drain as timed-out was invisible here.
    assert_eq!(
        collected,
        CapturedTail::complete(vec!["matrix sync failed, refusing to continue".to_string()]),
        "the dying worker's explanation must be collected, not raced — an empty tail here \
         is the `no stderr captured` line that made #730's fix contentless — and the \
         capture must report itself COMPLETE, or the renderers hedge a tail that is in \
         fact the worker's last word"
    );
}

#[test]
fn a_drain_that_never_finishes_is_reported_as_incomplete() {
    // #732: `collect_tail_after_drain` used to discard `wait_for_drain`'s
    // verdict, so this state was indistinguishable from a completed drain
    // and the renderers then asserted a diagnosis they had not earned.
    //
    // No `mark_drained` is ever called, so the wait burns its whole cap.
    let tail = StderrTail::new(8);
    tail.push("worker booting".to_string());

    let started = Instant::now();
    let collected = collect_tail_after_drain(&tail);
    let waited = started.elapsed();

    assert_eq!(
        collected.state(),
        TailState::FirstWords(&["worker booting".to_string()]),
        "a drain that never reached EOF must be reported as FIRST words; saying otherwise \
         lets `format_death_report` call a boot line the worker's most recent output. The \
         lines must still be there — an incomplete drain still yields whatever arrived, \
         since the tail is a bounded ring and not a transaction"
    );
    // POSITIVE CONTROL on the fixture itself: if the cap were not actually
    // being waited out, this test would pass for the wrong reason — a
    // `wait_for_drain` that returned `false` immediately is a different
    // (and also broken) implementation.
    assert!(
        waited >= TAIL_DRAIN_WAIT,
        "the collector must actually wait out its cap before giving up; waited {waited:?}, \
         cap is {TAIL_DRAIN_WAIT:?}"
    );
}


#[test]
fn an_unread_pipe_that_stops_draining_says_which_pipe_and_why() {
    // #888: the broker's STDOUT drain threw its `DrainEnd` away, so the one
    // line saying "this pipe is no longer drained" — the line an operator
    // chasing a hung broker needs — was never emitted for it. The warning must
    // name the pipe it is about (the broker drains both) and the fault.
    let fault = ReadFault::os(libc::EIO);
    let w = unread_drain_warning(DrainEnd::ReadError(fault), "stdout")
        .expect("a failed drain of an unread pipe must warn");
    assert!(w.starts_with("stdout drain ended on a READ ERROR"), "{w}");
    assert!(w.contains("this stdout is no longer drained"), "{w}");
    assert!(w.contains(&fault.to_string()), "the errno must be on the line: {w}");
    assert_eq!(
        unread_drain_warning(DrainEnd::Eof, "stdout"),
        None,
        "EOF is the process closing its pipe — nothing is left undrained"
    );
}

#[test]
fn a_torn_down_pty_is_a_read_error_on_linux_and_eof_on_macos() {
    // #754: every other `ReadError` in the tree comes from a fixture `Read`.
    // This one comes from the kernel: a pty master whose slave has closed —
    // the shape of #747's torn-down guest fd. The platforms differ, and the
    // test pins both, so nothing here may assume the Mac ever sees this arm.
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (-1, -1);
    use std::ptr::null_mut;
    // SAFETY: valid out-pointers; the name/termios/winsize args may be null.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, null_mut(), null_mut(), null_mut()) };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: `slave` is ours and closed exactly once; `master` moves into the File.
    unsafe { libc::close(slave) };
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let end = drain_reader(0, master, None);
    #[cfg(target_os = "linux")]
    assert_eq!(end, DrainEnd::ReadError(ReadFault::os(libc::EIO)));
    #[cfg(target_os = "macos")]
    assert_eq!(end, DrainEnd::Eof);
}
