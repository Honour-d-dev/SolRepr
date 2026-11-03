//! The type universe: semantic handles, syntactic type references, and the
//! conversion relation between them.
//!
//! - `ty` holds the two handles — `Ty` (every expression's type) and `Type`
//!   (the declarable subset).
//! - `kind` holds `TyKind`, the tuple type, and the conversion relation.
//! - `func` holds function signatures and the call-kind vocabulary.
//! - `literal` and `primitive` hold the leaf type families.
//! - `syntax` holds syntactic type references (`TyName`, `Path`), the
//!   declaration qualifiers, and the `TypeBuilder` lowering seam.

mod func;
mod kind;
mod literal;
mod primitive;
mod syntax;
mod ty;

pub use func::*;
pub use kind::*;
pub use literal::*;
pub use primitive::*;
pub use syntax::*;
pub use ty::*;
