//! Граф сущностей (Entity-Relation Graph) с K-hop обходом.
//! Порт из poler-engine: только entity_graph (K-hop связи якорей);
//! connectome/flyops/chem_kernel/graph_asm остались в движке.

pub mod connectome;
pub mod entity_graph;

pub use connectome::{Connectome, ConnectomeNodes, Edge, InEdges, KHopReport, SignFilter};
pub use entity_graph::{CodeSymbolRef, EntityGraph, IdentityPolicy, KnowledgeNode, RelationEdge};
