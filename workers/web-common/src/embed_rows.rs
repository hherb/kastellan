//! Shared normalisation of OpenAI-compatible embedding responses.
//!
//! Three embed paths decode a `{data:[{index, embedding}]}` response and must
//! reconcile it with the request: the trusted broker (`forward_embed`), and
//! web-research's two client embedders (`HttpEmbedder`, `BrokeredEmbedder`).
//! They all need the *same* three guarantees, so the rule lives here once:
//!
//! 1. **count** — exactly one vector per input,
//! 2. **order** — rows sorted by their 0-based `index` so vector `i` pairs with
//!    input `i` even when the backend returns them out of order,
//! 3. **contiguity** — after sorting, row `i` carries `index == i`; duplicate or
//!    gapped indices (e.g. `[0, 0]` or `[0, 2]` for two inputs) pass the count
//!    check yet would silently mispair a vector with the wrong input, so they
//!    are rejected fail-closed.
//!
//! Callers decode into `(index, embedding)` pairs and map [`ReorderError`] onto
//! their own error type.

/// Why an embedding response could not be reconciled with its request.
#[derive(Debug, PartialEq)]
pub enum ReorderError {
    /// The backend returned a different number of vectors than inputs sent.
    CountMismatch { requested: usize, returned: usize },
    /// After sorting by `index`, row `row` did not carry `index == row`
    /// (a duplicate or gapped index — the batch cannot be safely paired).
    NonContiguous { row: usize, index: usize },
}

impl std::fmt::Display for ReorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReorderError::CountMismatch { requested, returned } => write!(
                f,
                "vector count mismatch: requested {requested}, returned {returned}"
            ),
            ReorderError::NonContiguous { row, index } => write!(
                f,
                "non-contiguous embedding indices (row {row} has index {index})"
            ),
        }
    }
}

/// Reorder decoded embedding rows into input order, verifying exactly one
/// contiguous vector per input.
///
/// `rows` is `(index, embedding)` as decoded from the backend; `expected` is the
/// number of inputs sent. On success returns the embeddings in input order with
/// the index stripped. See the module docs for the three guarantees enforced.
///
/// An empty request (`expected == 0` with no rows) reconciles to an empty result.
pub fn reorder_embeddings(
    mut rows: Vec<(usize, Vec<f32>)>,
    expected: usize,
) -> Result<Vec<Vec<f32>>, ReorderError> {
    if rows.len() != expected {
        return Err(ReorderError::CountMismatch { requested: expected, returned: rows.len() });
    }
    rows.sort_by_key(|(index, _)| *index);
    for (row, (index, _)) in rows.iter().enumerate() {
        if *index != row {
            return Err(ReorderError::NonContiguous { row, index: *index });
        }
    }
    Ok(rows.into_iter().map(|(_, embedding)| embedding).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_rows_pass_through() {
        let rows = vec![(0, vec![1.0, 2.0]), (1, vec![3.0, 4.0])];
        assert_eq!(reorder_embeddings(rows, 2).unwrap(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn out_of_order_rows_are_sorted_to_input_order() {
        let rows = vec![(1, vec![3.0, 4.0]), (0, vec![1.0, 2.0])];
        assert_eq!(reorder_embeddings(rows, 2).unwrap(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn too_few_rows_is_count_mismatch() {
        let rows = vec![(0, vec![1.0])];
        assert_eq!(
            reorder_embeddings(rows, 2),
            Err(ReorderError::CountMismatch { requested: 2, returned: 1 })
        );
    }

    #[test]
    fn duplicate_index_is_non_contiguous() {
        // Count matches (2 rows for 2 inputs) but both claim index 0.
        let rows = vec![(0, vec![1.0]), (0, vec![2.0])];
        assert_eq!(
            reorder_embeddings(rows, 2),
            Err(ReorderError::NonContiguous { row: 1, index: 0 })
        );
    }

    #[test]
    fn gapped_index_is_non_contiguous() {
        // Count matches but indices are {0, 2}: position 1 is unfilled.
        let rows = vec![(0, vec![1.0]), (2, vec![2.0])];
        assert_eq!(
            reorder_embeddings(rows, 2),
            Err(ReorderError::NonContiguous { row: 1, index: 2 })
        );
    }

    #[test]
    fn empty_request_reconciles_to_empty() {
        assert_eq!(reorder_embeddings(Vec::new(), 0).unwrap(), Vec::<Vec<f32>>::new());
    }
}
