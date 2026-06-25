//! The provider-neutral model seam.
//!
//! One `call_model()` seam, no provider lock-in. Anthropic-specific features
//! (prompt caching, budgets, effort) are one provider profile behind this
//! seam, not the design center. The default system prompt is bare: the
//! zero-default-context floor.
