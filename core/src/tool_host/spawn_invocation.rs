//! Pure helper that builds the `(program, args)` pair to spawn for a worker,
//! honoring an optional lockdown shim.
//!
//! When `shim` is `Some`, the worker binary runs *through* the shim
//! (`kastellan-worker-lockdown-exec`): the shim applies the prelude lockdown
//! then `execve`s the real binary, which inherits the seccomp filter. This is
//! how pure-Python venv workers (browser-driver) get worker-side seccomp on
//! Linux, where bwrap spawns them directly and never runs the Rust prelude.
//! When `None`, the binary is spawned directly — every Rust worker, which
//! locks itself down via `serve_stdio`.

use std::path::Path;

/// Build `(program, args)` for the sandbox spawn. Owned returns so callers can
/// borrow `&str` into a `WorkerSpec`. `base_args` is the worker's own argv
/// (empty for every current worker).
pub fn build_program_and_args(
    binary: &Path,
    shim: Option<&Path>,
    base_args: &[&str],
) -> (String, Vec<String>) {
    match shim {
        Some(shim) => {
            let program = shim.to_string_lossy().into_owned();
            let mut args = Vec::with_capacity(base_args.len() + 1);
            args.push(binary.to_string_lossy().into_owned());
            args.extend(base_args.iter().map(|a| a.to_string()));
            (program, args)
        }
        None => (
            binary.to_string_lossy().into_owned(),
            base_args.iter().map(|a| a.to_string()).collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn no_shim_spawns_binary_directly() {
        let (program, args) = build_program_and_args(Path::new("/venv/bin/worker"), None, &[]);
        assert_eq!(program, "/venv/bin/worker");
        assert!(args.is_empty());
    }

    #[test]
    fn no_shim_preserves_base_args() {
        let (program, args) =
            build_program_and_args(Path::new("/venv/bin/worker"), None, &["--x", "y"]);
        assert_eq!(program, "/venv/bin/worker");
        assert_eq!(args, vec!["--x".to_string(), "y".to_string()]);
    }

    #[test]
    fn shim_wraps_binary_as_first_arg() {
        let (program, args) = build_program_and_args(
            Path::new("/venv/bin/worker"),
            Some(Path::new("/usr/bin/kastellan-worker-lockdown-exec")),
            &["--flag"],
        );
        assert_eq!(program, "/usr/bin/kastellan-worker-lockdown-exec");
        assert_eq!(
            args,
            vec!["/venv/bin/worker".to_string(), "--flag".to_string()]
        );
    }

    #[test]
    fn shim_wraps_binary_with_no_extra_args() {
        // The production case for every current Python worker: base_args empty,
        // so the shim's only arg is the worker binary it execs.
        let (program, args) = build_program_and_args(
            Path::new("/venv/bin/worker"),
            Some(Path::new("/usr/bin/kastellan-worker-lockdown-exec")),
            &[],
        );
        assert_eq!(program, "/usr/bin/kastellan-worker-lockdown-exec");
        assert_eq!(args, vec!["/venv/bin/worker".to_string()]);
    }
}
