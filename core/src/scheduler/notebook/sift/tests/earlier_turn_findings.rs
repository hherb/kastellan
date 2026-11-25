//! The sift loop's cases for a table over an earlier turn's scrapbook
//! (#956), run by [`super::the_sift_loop`] on its cluster.

use serde_json::json;

use super::*;

/// #956: an earlier turn's table took its columns from the first row's
/// keys, which come back alphabetical. The conversation's record of the
/// turn keeps the sift's own `fields` order, so the table shows the
/// columns as that sift asked for them.
pub(super) async fn an_earlier_turns_table_keeps_its_sifts_column_order(pool: &sqlx::PgPool) {
    let earlier = new_task(pool, 5).await.task_id;
    let row = scrapbook::NewRow {
        kind: RowKind::Kept,
        source: json!({"id": "e1"}),
        fields: json!({"vendor": "Acme", "gross_amount": 120, "id": "INV-7"}),
        classification: "Personal".into(),
    };
    scrapbook::append_rows(pool, earlier, 2, &[row]).await.unwrap();
    let r = format!("sb:{earlier}:2");
    let mut ctx = new_task(pool, 5).await;
    ctx.conversation_task_ids = Some(vec![earlier]);
    ctx.conversation = vec![json!({"at": "t", "user": "my invoices", "answer": "a",
        "scrapbooks": [{"scrapbook": r, "criterion": "an invoice", "fields": ["vendor", "gross_amount", "id"], "rows": 1}]})];
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "As before:", "table": {"scrapbook": r}}})]);
    let b = body(&run(pool, &script, ctx).await.result);
    assert!(b.contains("| vendor | gross_amount | id | source |") && b.contains("| Acme | 120 | INV-7 | e1 |"), "{b}");
}

/// #956: a table naming an earlier turn's chain by its last link showed
/// only that link's rows, because the chain was read from this run's
/// summaries alone. The turn record keeps each link's `continues`.
pub(super) async fn an_earlier_turns_chain_is_tabled_whole(pool: &sqlx::PgPool) {
    let earlier = new_task(pool, 5).await.task_id;
    for (sift, vendor) in [(2, "Acme"), (3, "Globex")] {
        let row = scrapbook::NewRow {
            kind: RowKind::Kept,
            source: json!({"id": vendor}),
            fields: json!({"vendor": vendor}),
            classification: "Personal".into(),
        };
        scrapbook::append_rows(pool, earlier, sift, &[row]).await.unwrap();
    }
    let (first, last) = (format!("sb:{earlier}:2"), format!("sb:{earlier}:3"));
    let mut ctx = new_task(pool, 5).await;
    ctx.conversation_task_ids = Some(vec![earlier]);
    ctx.conversation = vec![json!({"at": "t", "user": "my invoices", "answer": "a", "scrapbooks": [
        {"scrapbook": first, "criterion": "an invoice", "fields": ["vendor"], "rows": 1},
        {"scrapbook": last, "criterion": "an invoice", "fields": ["vendor"], "rows": 1, "continues": first},
    ]})];
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "As before:", "table": {"scrapbook": last}}})]);
    let b = body(&run(pool, &script, ctx).await.result);
    assert!(b.contains("| Acme | Acme |") && b.contains("| Globex | Globex |"), "{b}");
}

/// #956: an earlier turn's table counted only its rows as looked at ("2
/// kept of 2 looked at" for sifts that judged 80 items between them). The
/// record keeps each sift's `looked_at`, `remaining` and `from`; a chain's
/// earlier link leaves its rest to the link that went on from it. A link
/// that kept nothing is still counted: its record has its counts.
pub(super) async fn an_earlier_turns_table_counts_what_its_sifts_looked_at(pool: &sqlx::PgPool) {
    let earlier = new_task(pool, 5).await.task_id;
    let row = scrapbook::NewRow {
        kind: RowKind::Kept,
        source: json!({"id": "Globex"}),
        fields: json!({"vendor": "Globex"}),
        classification: "Personal".into(),
    };
    scrapbook::append_rows(pool, earlier, 3, &[row]).await.unwrap();
    let (first, last) = (format!("sb:{earlier}:2"), format!("sb:{earlier}:3"));
    let mut ctx = new_task(pool, 5).await;
    ctx.conversation_task_ids = Some(vec![earlier]);
    ctx.conversation = vec![json!({"at": "t", "user": "my invoices", "answer": "a", "scrapbooks": [
        {"scrapbook": first, "fields": ["vendor"], "rows": 0, "looked_at": 30, "remaining": 50},
        {"scrapbook": last, "fields": ["vendor"], "rows": 1, "continues": first, "from": 30, "looked_at": 30, "remaining": 20},
    ]})];
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "As before:", "table": {"scrapbook": last}}})]);
    let b = body(&run(pool, &script, ctx).await.result);
    assert!(b.contains("1 kept of 60 looked at") && b.contains("20 not yet looked at"), "{b}");
    assert!(!b.contains("could not be included"), "{b}");
}
