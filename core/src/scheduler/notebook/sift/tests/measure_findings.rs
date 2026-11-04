//! The sift loop's cases added for the DGX measures (tasks 301–307, 322), run by
//! [`super::the_sift_loop`] on its cluster.

use serde_json::{json, Value};

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
    // The last link looks at 1 and keeps 0, so swapped counts would show.
    .sorting(&[keep(10), keep(20), no()]);
    script.sift_cfg.max_reads = 2;
    let r = run(pool, &script, ctx).await;
    let second = summary_seen(&script, 3);
    assert_eq!(second["continues"], json!(format!("sb:{}:2", r.task_id)), "{second}");
    assert_eq!(second["chain"], json!({"sifts": 2, "looked_at": 3, "kept": 2}), "the planner sees the chain's counts: {second}");
    let first = summary_seen(&script, 2);
    assert!(first.get("continues").is_none() && first.get("chain").is_none(), "the first sift is no chain: {first}");
    let b = body(&r.result);
    let (hi, lo) = (b.find("| 20 | m2 |").expect(&b), b.find("| 10 | m1 |").expect(&b));
    assert!(hi < lo, "both links' rows, ordered: {b}");
    assert!(b.ends_with("2 kept of 3 looked at."), "the chain's counts, nothing left over: {b}");
}

/// DGX task 322: a sift over a 27-row scrapbook `from` 64 (a search's
/// `next_from`) ran as an empty success. A `from` past the end is refused,
/// naming the list's length, before anything is sorted.
pub(super) async fn a_sift_from_past_the_end_is_refused_before_sorting(pool: &sqlx::PgPool) {
    let mut late = sift(2, None);
    late["parameters"]["from"] = json!(5);
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2"]))}),
        json!({"tick": tick(1, "listed"), "call": late}),
        json!({"infeasible": {"reason": "nothing left"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert!(kinds(pool, r.task_id).await.contains(&EntryKind::FailedStep));
    let v: Value = serde_json::from_str(&script.messages()[2]).unwrap();
    let blocks = v["blocks"].to_string();
    assert!(blocks.contains("sift_refused") && blocks.contains("(2 items)"), "{blocks}");
    assert!(script.sorted.lock().unwrap().is_empty(), "nothing was sorted");
}
