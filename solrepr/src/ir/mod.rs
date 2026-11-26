/// The IR module is for structures that sit somewhere between the Ast layer and the HIR layer
/// They dont quite belong on either edge so this is their home.

pub mod def_map;
pub mod import_map;
pub mod item_tree;
pub mod membership;

#[cfg(test)]
mod tests;
