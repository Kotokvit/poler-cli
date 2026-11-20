//! poler — Sovereign CLI Suite: Harvester, Grep, Zero-Copy Piece-Table Editor & AI-Agent MCP Engine

pub mod core;
pub mod grep;
pub mod harvest;
pub mod mcp;

pub use crate::core::{
    agent_ops, buffer, crystal, engine_gate, event_plane, extractor, fastcdc, fsst, license,
    pdf_edit, protocol, scratch, search, trit_silicon, trit_syntax, universal_letters, vfs,
    vfs_edit,
};
