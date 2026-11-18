//! GiCo - context-aware terminal assistant library.
//!
//! The context engine ([`context`]) is a reusable component: it collects a
//! verified snapshot of the local environment (OS, shell, project, files,
//! git metadata) and renders a compact prompt for small local models.
//! [`verify`] checks generated commands against the real filesystem.
//! [`llm`] talks to local inference servers (Ollama, llama.cpp) in a
//! model-agnostic way.

pub mod approve;
pub mod config;
pub mod constraints;
pub mod context;
pub mod discovery;
pub mod harness;
pub mod intent;
pub mod interact;
pub mod launch;
pub mod lifecycle;
pub mod llm;
pub mod plan;
pub mod runtime;
pub mod shell;
pub mod ui;
pub mod verify;

mod launch_adapters;
mod query;

pub mod auth;
pub mod responses;
pub mod usage;
