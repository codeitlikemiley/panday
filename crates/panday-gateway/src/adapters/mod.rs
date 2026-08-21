//! Provider adapters — a small, sealed set (docs/11).
//!
//! Deliberately NOT a plugin surface: supply-chain risk and dialect drift
//! belong to us. Four dialects total (`anthropic`, `openai`, `openai_compat`,
//! `local`); `local` is `openai_compat` pinned to loopback with no auth.

pub mod anthropic;
pub mod openai_compat;
pub mod pool;
