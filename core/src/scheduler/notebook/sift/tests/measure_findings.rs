//! The sift loop's cases added for the DGX measure of tasks 301–307, run by
//! [`super::the_sift_loop`] on its cluster.

use serde_json::json;

use super::*;

/// DGX tasks 302 and 307: a find-all took a chain of sifts, each `from` the
/// last one's `next_from`, and the answer tabled only the last. A sift that
/// goes on says `continues`, and a table naming the last shows them all,
/// with the counts of the whole chain.
pub(super) async fn a_table_naming_the_last_of_a_chain_shows_every_link(pool: &sqlx::PgPool) {
    let ctx = new_task(pool, 5).await;
    let sift1 = |from: usize| {
        let mut c = sift(1, None);
        c["parameters"]["from"] = json!(from);
        c
    };
    let table = json!({"scrapbook": format!("sb:{}:3", ctx.task_id), "order_by": {"column": "amount", "desc": true}});
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2", "m3"]))}),
        json!({"call": sift1(0)}),
        json!({"call": sift1(2)}),
        json!({"tick": tick(1, "sorted"), "answer": {"body": "Your invoices:", "table": table}}),
    ])
    .sorting(&[keep(10), no(), keep(30)]);
    script.sift_cfg.max_reads = 2;
    let r = run(pool, &script, ctx).await;
    let second = summary_seen(&script, 3);
    assert_eq!(second["continues"], json!(format!("sb:{}:2", r.task_id)), "{second}");
    assert!(summary_seen(&script, 2).get("continues").is_none(), "the first sift continues nothing");
    let b = body(&r.result);
    let (hi, lo) = (b.find("| 30 | m3 |").expect(&b), b.find("| 10 | m1 |").expect(&b));
    assert!(hi < lo, "both links' rows, ordered: {b}");
    assert!(b.ends_with("2 kept of 3 looked at."), "the chain's counts, nothing left over: {b}");
}
