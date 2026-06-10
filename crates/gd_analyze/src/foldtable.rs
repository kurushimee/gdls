//! The per-node constant-folding side table.
//!
//! Godot's `ExpressionNode` carries `bool reduced` + `Variant reduced_value`. Those aren't in
//! `gd_syntax`'s parser-only AST, so folded constant values live here, parallel to
//! [`crate::typetable::TypeTable`] and indexed the same way. Used by constant contexts (annotation
//! arguments, `const` initializers) where the analyzer must evaluate an expression to a value.

use crate::data_type::VariantType;
use gd_syntax::ast::NodeId;

/// A folded compile-time constant. The subset of `Variant` the analyzer needs in constant contexts;
/// `Array`/`Dictionary` folding is added with the `make_*_reduced_value` family (WP-F).
#[derive(Clone, Debug, PartialEq)]
pub enum FoldedValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// A constant whose value the analyzer cannot materialize — builtin named constants like
    /// `Vector3.UP` (Godot folds the real `Variant` via `Variant::get_constant_value`,
    /// analyzer.cpp:4059-4067; this subset has no vector/color representations). Carries the
    /// value's [`VariantType`] so error paths can still name the operand's kind. Participates in
    /// `is_reduced` (constancy gates) but never in value-dependent folding: binary ops validate by
    /// type instead of evaluating, and dup-key checks treat an unknown value as never-equal.
    Opaque(VariantType),
}

/// `NodeId` → its folded constant value, if the expression reduced to one.
#[derive(Debug, Default)]
pub struct FoldTable {
    values: Vec<Option<FoldedValue>>,
}

impl FoldTable {
    pub fn new(node_count: usize) -> Self {
        FoldTable {
            values: vec![None; node_count],
        }
    }

    pub fn set(&mut self, id: NodeId, value: FoldedValue) {
        self.values[id.index()] = Some(value);
    }

    /// The folded value of a node, or `None` if it isn't a known constant.
    pub fn get(&self, id: NodeId) -> Option<&FoldedValue> {
        self.values[id.index()].as_ref()
    }

    /// Whether the node reduced to a constant (Godot's `reduced` flag).
    pub fn is_reduced(&self, id: NodeId) -> bool {
        self.values[id.index()].is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_none_then_records() {
        let id = NodeId::default();
        let mut table = FoldTable::new(4);
        assert!(!table.is_reduced(id));
        assert_eq!(table.get(id), None);
        table.set(id, FoldedValue::Int(42));
        assert!(table.is_reduced(id));
        assert_eq!(table.get(id), Some(&FoldedValue::Int(42)));
    }
}
