//! GiCo - context-aware terminal assistant library.
//!
//! The context engine ([`context`]) is a reusable component: it collects a
//! verified snapshot of the local environment (OS, shell, project, files,
//! git metadata) and renders a compact prompt for small local models.
//! [`verify`] checks generated commands against the real filesystem.
//! [`llm`] talks to local inference servers (Ollama, llama.cpp) in a
//! model-agnostic way.

pub mod approve;
pub mod context;
pub mod harness;
pub mod intent;
pub mod interact;
pub mod llm;
pub mod plan;
pub mod runtime;
pub mod ui;
pub mod verify;
