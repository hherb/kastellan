//! What a table's scrapbooks looked at and left (#1015, #956): each
//! included scrapbook as a [`Link`], its list's items counted once
//! ([`coverage`]), and a line for each question that stopped part way
//! ([`stop_lines`]). Split out of `answer.rs`. Pure.

use super::super::params::{ScrapRef, SiftParams};

/// An included scrapbook: the list it went over, the span of it it looked
/// at, and what it left.
pub(super) struct Link {
    /// Its ref, `sb:<task>:<sift>`.
    pub(super) name: String,
    /// The link it continues, if it is a chain's later link.
    pub(super) continues: Option<String>,
    /// Its list's key ([`SiftParams::list_key`]): sifts with the same went
    /// through one list. `None` for an earlier turn's ref whose record has
    /// none (stored before issue 1058, or its parameters did not parse): a
    /// list of its own.
    pub(super) list: Option<String>,
    pub(super) from: usize,
    pub(super) looked_at: usize,
    pub(super) remaining: usize,
}

impl Link {
    /// From `facts` (this run's summary, else an earlier turn's record, #956)
    /// and the parameters this run has. A record stored before its counts
    /// has only rows: "looked at" is kept plus withheld, a lower bound.
    pub(super) fn of(name: String, facts: Option<&serde_json::Value>, params: Option<SiftParams>, rows: usize) -> Self {
        let n = |k: &str| facts.and_then(|s| s[k].as_u64()).map(|v| v as usize);
        let from = params.as_ref().map_or_else(|| n("from").unwrap_or(0), |p| p.from);
        let list = match params {
            Some(p) => ScrapRef::parse(&name).ok().map(|r| p.list_key(r.task_id)),
            None => facts.and_then(|s| s["list"].as_str()).map(String::from),
        };
        Link {
            name,
            continues: facts.and_then(|s| s["continues"].as_str()).map(String::from),
            from,
            list,
            looked_at: n("looked_at").unwrap_or(rows),
            remaining: n("remaining").unwrap_or(0),
        }
    }
}

/// The items the table's scrapbooks looked at and did not, each list
/// counted once (#1015): two sifts over one 80-item list that each looked
/// at its first 30 looked at 30, not 60, and left 50, not 100. A chain's
/// links are adjacent spans of one list, so they add up; a link left out
/// (the table's cap, an unreadable one) leaves its span unread. Pure.
///
/// "One list" is one [`SiftParams::list_key`]; see its shortcut for a step
/// re-run between two sifts (#959). A sift's
/// unread span that another covered is not counted here: [`stop_lines`]
/// names it (issue 1026).
pub(super) fn coverage(links: &[Link]) -> (usize, usize) {
    let mut seen: Vec<&String> = Vec::new();
    let (mut looked, mut left) = (0, 0);
    for l in links {
        let group: Vec<&Link> = match &l.list {
            // An earlier turn's ref with no key, its own list; a link another went on from leaves its rest to it.
            None => {
                looked += l.looked_at;
                if !links.iter().any(|o| o.continues.as_ref() == Some(&l.name)) {
                    left += l.remaining;
                }
                continue;
            }
            Some(k) if seen.contains(&k) => continue,
            Some(k) => {
                seen.push(k);
                links.iter().filter(|o| o.list.as_ref() == Some(k)).collect()
            }
        };
        let (a, b) = list_coverage(&group);
        looked += a;
        left += b;
    }
    (looked, left)
}

/// One list's: the union of the spans its sifts looked at
/// (`from..from + looked_at`), and the rest of the list, whose length is the
/// longest any of them saw (`from + looked_at + remaining`). Pure.
fn list_coverage(group: &[&Link]) -> (usize, usize) {
    let len = group.iter().map(|l| l.from + l.looked_at + l.remaining).max().unwrap_or(0);
    let mut spans: Vec<(usize, usize)> = group.iter().map(|l| (l.from, l.from + l.looked_at)).collect();
    spans.sort_unstable();
    let (mut looked, mut end) = (0, 0);
    for (start, stop) in spans {
        let start = start.max(end);
        if stop > start {
            looked += stop - start;
            end = stop;
        }
    }
    (looked, len.saturating_sub(looked))
}

/// Issue 1026 (d): when a table joins more than one question (sifts no
/// later included link continues), a line for each that stopped with items
/// unread — the note's "not yet looked at" is per list, so another question
/// covering those items hides that this one never judged them. Pure.
pub(super) fn stop_lines(links: &[Link]) -> Vec<String> {
    let last: Vec<&Link> = links.iter().filter(|l| !links.iter().any(|o| o.continues.as_ref() == Some(&l.name))).collect();
    if last.len() < 2 {
        return Vec::new();
    }
    last.iter()
        .filter(|l| l.remaining > 0)
        .map(|l| format!("(sift {} stopped after {} of its {} items)", l.name, l.from + l.looked_at, l.from + l.looked_at + l.remaining))
        .collect()
}
