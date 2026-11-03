pub mod access;
pub mod analysis;
pub mod body_map;
pub mod builtins;
pub mod docs;
pub mod exprs;
pub mod inference;
pub mod item_data;
pub mod member_lookup;
pub mod member_map;
pub mod overrides;
pub mod resolution;
pub mod resolver;
pub mod semantic_errors;
pub mod signatures;
pub mod types;
pub mod linearize;

#[cfg(test)]
pub mod tests;
