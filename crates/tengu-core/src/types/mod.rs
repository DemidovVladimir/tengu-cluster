//! Transport-neutral runtime types shared by channels and engines.
//!
//! Potential use case:
//! Re-export one module from app code to access canonical message and stream event types.

pub mod message;
pub mod stream;

pub use message::*;
pub use stream::*;
