//! `memory l3 revoke <id>` — downgrade a crystallised (layer-3) skill's
//! trust back to `untrusted` (no approval gate; revocation is always safe),
//! emitting the `actor='cli'` audit trail.

use std::process::ExitCode;

use crate::common::resolve_connect_spec;

pub(super) async fn memory_l3_revoke(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::l3_revoke_and_audit;
    use kastellan_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: kastellan-cli memory l3 revoke <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 revoke: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match l3_revoke_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("revoked id={id} → trust=untrusted"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 3 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l3 revoke: {e}"); ExitCode::from(1) }
    }
}
