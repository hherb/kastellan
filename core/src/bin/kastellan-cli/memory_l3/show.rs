//! `memory l3 show <id>` — print the full payload of a crystallised skill so
//! the operator can READ it before approving. For a Python skill that is the
//! verbatim source (the human read IS the approval gate); for a templated
//! skill it is the pretty-printed step template.

use std::fmt::Write as _;
use std::process::ExitCode;

use super::shared::load_skill_row;

/// Render a string for safe display in the operator's terminal: any control
/// character other than those in `keep` is replaced with a visible `\xNN`
/// escape (one per UTF-8 byte). `validate_python_skill` already rejects these
/// at crystallise/approve, so this is defense-in-depth: a hand-edited SQL row
/// must NOT be able to inject terminal escape sequences (ESC, CR, …) into the
/// review surface, because `show` is the human approval gate.
fn sanitize_for_terminal(s: &str, keep: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_control() && !keep.contains(&ch) {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "\\x{b:02x}");
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub(super) async fn memory_l3_show(args: &[String]) -> ExitCode {
    let (_, row) = match load_skill_row(args, "show").await {
        Ok(x) => x,
        Err(code) => return code,
    };
    let kind = row.metadata.get("kind").and_then(|v| v.as_str()).unwrap_or("templated");
    let trust = kastellan_core::memory::l3_approval::SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .as_str();
    println!("# skill #{} (kind={kind}, trust={trust})", row.id);
    // Description is single-line by validation; escape ALL controls defensively.
    println!("# description: {}", sanitize_for_terminal(&row.body, &[]));
    match kind {
        "python" => match row.metadata.get("python").and_then(|p| p.get("code")).and_then(|v| v.as_str()) {
            Some(code) => {
                // Keep newline + tab (legitimate Python source layout); escape
                // every other control char so the operator reads the true code.
                let safe = sanitize_for_terminal(code, &['\n', '\t']);
                println!("--- code ---");
                print!("{safe}");
                if !safe.ends_with('\n') {
                    println!();
                }
            }
            None => {
                eprintln!("memory l3 show: id={} has kind=python but no python.code", row.id);
                return ExitCode::from(1);
            }
        },
        _ => match row.metadata.get("template") {
            Some(t) => {
                println!("--- template ---");
                println!("{}", serde_json::to_string_pretty(t).unwrap_or_else(|_| t.to_string()));
            }
            None => {
                eprintln!("memory l3 show: id={} has no template", row.id);
                return ExitCode::from(1);
            }
        },
    }
    ExitCode::from(0)
}
