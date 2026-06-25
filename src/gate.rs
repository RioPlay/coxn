//! The gate: aden's blast-radius contract.
//!
//! Before the pump accepts an edit it consults aden's `impact-diff --scope`
//! verdict and obeys the exit code: in-scope (proceed), scope-escape or
//! blast-leak (block, surface the verdict). The contract types that coxn
//! consumes are defined here. See docs/contract.adoc.
