//! Which items a sift takes, and which of its reads belong to each (#922).
//! Pure.

use serde_json::Value;

use super::params::ReadTemplate;
use super::template::expand_reads;

/// The reads to make, and which of them belong to each item taken.
#[derive(Debug, PartialEq)]
pub(crate) struct Batch {
    pub reads: Vec<Value>,
    /// Item `i`'s reads are `reads[per_item[i]]`. One entry per item taken.
    pub per_item: Vec<std::ops::Range<usize>>,
    /// Reads of the first item dropped to fit `cap` (see [`plan_reads`]).
    pub cut: usize,
}

/// Take items in list order while their reads fit in `cap`, and never more
/// than `cap` items: an item with no read still costs a sorter call. An
/// item's reads stay together. A first item with more reads than `cap` alone
/// keeps its first `cap` (the rest counted in `cut`), so a sift always makes
/// progress. Pure.
pub(crate) fn plan_reads(read: Option<&ReadTemplate>, items: &[Value], cap: usize) -> Batch {
    let mut batch = Batch { reads: Vec::new(), per_item: Vec::new(), cut: 0 };
    for item in items {
        if batch.per_item.len() >= cap.max(1) {
            break;
        }
        let mut reads = read.map(|t| expand_reads(t, item)).unwrap_or_default();
        if batch.reads.len() + reads.len() > cap {
            if !batch.per_item.is_empty() {
                break;
            }
            batch.cut = reads.len() - cap;
            reads.truncate(cap);
        }
        let start = batch.reads.len();
        batch.reads.extend(reads);
        batch.per_item.push(start..batch.reads.len());
    }
    batch
}

#[cfg(test)]
mod tests;
