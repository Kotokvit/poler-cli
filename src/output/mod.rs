//! AI-Ready выходной контракт ContextAnchor (порт из poler-engine).

pub mod context_anchor;

pub use context_anchor::{
    render_markdown, render_simple, ContextAnchor, NexusNode, NexusSite, SearchResult,
};
