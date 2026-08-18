//! Provider dialects.
//!
//! docs/02 puts "providers, middleware" in `ferrum-sdk`, and docs/10 requires
//! that "the same stack runs inside the gateway's adapters (write once, use
//! both sides)". These modules are the wire layer: they turn the model IR into
//! a provider's dialect and its stream back into `StreamItem`s.
//!
//! What lives here is dialect + transport only. Adapter *policy* — declared
//! capabilities, the sealed adapter registry, routing and ledger context —
//! belongs to `ferrum-gateway` (docs/11).

pub mod openai_compat;
