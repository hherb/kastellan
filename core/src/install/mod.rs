//! Operator installer for a per-user supervised Kastellan
//! (`kastellan-cli install`). Pure layout/spec planning in [`plan`];
//! the IO orchestration (copy, db-init, supervisor install, verify)
//! is added alongside.

pub mod plan;
