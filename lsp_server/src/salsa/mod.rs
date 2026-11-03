pub mod db;
pub mod files;
pub mod hir_db;
pub mod incremental_parser;
pub mod interned_db;
pub mod root_db;

pub use db::Db;
pub use db::{File, FileId, DbSnapshot, SourceRootId};
pub use hir_db::HirDb;
pub use root_db::RootDb;

#[cfg(test)]
mod tests;
