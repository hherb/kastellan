//! `memory l3 show <id>` — print the full payload of a crystallised skill so
//! the operator can READ it before approving. For a Python skill that is the
//! verbatim source (the human read IS the approval gate); for a templated
//! skill it is the pretty-printed step template.

use std::process::ExitCode;

use super::shared::load_skill_row;

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
    println!("# description: {}", row.body);
    match kind {
        "python" => match row.metadata.get("python").and_then(|p| p.get("code")).and_then(|v| v.as_str()) {
            Some(code) => {
                println!("--- code ---");
                print!("{code}");
                if !code.ends_with('\n') {
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
