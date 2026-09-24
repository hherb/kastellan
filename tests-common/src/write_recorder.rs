//! A `Write` sink that records each `write` call as its own chunk (#755).
//!
//! An evidence marker is only safe under `--nocapture` if it reaches the pipe
//! in ONE write that begins with `\n`: the newline ends libtest's partial
//! `test <name> ... ` line, and a single write of at most `PIPE_BUF` bytes to a
//! pipe is atomic, so nothing can land between the newline and the marker. (A
//! line longer than `PIPE_BUF` — 512 on macOS — loses that promise: POSIX
//! allows it to interleave anywhere. In practice its head, where the newline
//! and marker sit, goes first, but nothing guarantees it.) A
//! `Vec<u8>` sink concatenates writes and so cannot tell
//! `writeln!(out, "{}", s)` (two writes: the text, then `\n`) from one framed
//! `write!`. This can.

/// Records every `write` call separately. Each call is accepted whole.
#[derive(Default)]
pub(crate) struct WriteRecorder {
    chunks: Vec<String>,
}

impl std::io::Write for WriteRecorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.chunks.push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl WriteRecorder {
    /// Assert the emitter made exactly `markers.len()` writes, the i-th a
    /// framed `markers[i]` line. For an emitter that legitimately writes more
    /// than one marker, each of which must still be its own framed write.
    pub(crate) fn assert_framed_lines(&self, markers: &[&str]) {
        assert_eq!(
            self.chunks.len(),
            markers.len(),
            "expected one framed write per marker {markers:?}: {:?}",
            self.chunks
        );
        for (chunk, marker) in self.chunks.iter().zip(markers) {
            assert!(
                chunk.starts_with(&format!("\n{marker} ")) && chunk.ends_with('\n'),
                "each write must be framed `\\n{marker} …\\n`: {chunk:?}"
            );
        }
    }

    /// Assert the emitter made exactly one write, and that it is a framed
    /// `marker` line: `\n<marker> …\n`. Returns that write for further checks.
    pub(crate) fn assert_one_framed_line(&self, marker: &str) -> &str {
        assert_eq!(
            self.chunks.len(),
            1,
            "a marker must reach the pipe in ONE write, or another thread's output can \
             land between its newline and its marker: {:?}",
            self.chunks
        );
        let only = &self.chunks[0];
        assert!(
            only.starts_with(&format!("\n{marker} ")) && only.ends_with('\n'),
            "the write must be framed `\\n{marker} …\\n`, so it owns column 0 whatever \
             libtest left on the line: {only:?}"
        );
        only
    }
}
