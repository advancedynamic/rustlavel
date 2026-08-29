//! The providers that ship with the framework.
//!
//! Each one owns its wire format and nothing else: the same [`Request`] goes
//! in, the same [`Completion`] comes out, and every difference between the
//! three APIs is confined to one file.
//!
//! [`Request`]: crate::Request
//! [`Completion`]: crate::Completion

pub mod anthropic;
pub mod ollama;
pub mod openai;

pub use anthropic::Anthropic;
pub use ollama::Ollama;
pub use openai::OpenAi;
