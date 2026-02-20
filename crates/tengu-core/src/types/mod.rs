//! Transport-neutral runtime types shared by channels and engines.
//!
//! Potential use case:
//! Re-export one module from app code to access canonical message, stream,
//! and inter-agent handoff contract types.

pub mod handoff;
pub mod message;
pub mod stream;

pub use handoff::*;
pub use message::*;
pub use stream::*;
