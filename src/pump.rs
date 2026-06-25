//! The pump: steers and sets pace, carries no intelligence.
//!
//! The manual agentic loop lives here: call the model, dispatch tools, feed
//! results back, repeat. It paces turns and enforces the gate, but never
//! reasons about code. aden directs and gates; the LLM acts; the pump steers.
