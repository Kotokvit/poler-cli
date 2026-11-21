//! AIDDE: символьный уровень кода (def/call сайты) для ε-скан-пути.
//! Порт из poler-engine без impact/sqlite_store (они тянут СУБД-слой).

pub mod symbols;

pub use symbols::{CallSite, Definition, ImportStmt, SymbolTable};
