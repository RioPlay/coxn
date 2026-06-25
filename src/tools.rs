//! Thin tool dispatch.
//!
//! Maps a model-requested tool call to a result. Commodity machinery kept
//! deliberately thin. aden's tools are not injected up front; they are
//! discovered by intent through a deferred-loading seam (Phase 2).
