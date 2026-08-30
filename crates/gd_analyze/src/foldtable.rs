//! The per-node constant-folding side table.
//!
//! Godot's `ExpressionNode` carries three fields — `bool reduced` (a visited memo the reducer
//! stamps on entry, analyzer.cpp:5285), `bool is_constant`, and `Variant reduced_value`. None are
//! in `gd_syntax`'s parser-only AST, so the two that matter to the analyzer live here, parallel to
//! [`crate::typetable::TypeTable`] and indexed the same way. Used by constant contexts (annotation
//! arguments, `const` initializers) where the analyzer must evaluate an expression to a value, and
//! by the constancy GATES that only need to know an expression is constant.
//!
//! The two are separate sets, and confusing them is #364. Godot always sets `is_constant` and
//! `reduced_value` together, but gdls's [`FoldedValue`] cannot represent every `Variant` Godot
//! folds — a class object and a preloaded resource have no representation here at all. Those nodes
//! carry the bit with no value. So a gate reads [`FoldTable::is_constant`] and anything that
//! dereferences a value reads [`FoldTable::get`]; neither set can contaminate the other.

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
    /// `is_reduced` but never in value-dependent folding: binary ops validate by type instead of
    /// evaluating.
    ///
    /// The second field is `Some` only for a bare utility-function reference — the constant
    /// `Callable` Godot folds as `Callable(GDScriptUtilityCallable(name))`. Carrying that identity
    /// lets same-utility dictionary keys (`{print: 1, print: 2}`) be recognized as the same key,
    /// while every other opaque constant (`None`) still compares as never-equal (its value is
    /// genuinely unknown, so it can't be *proven* a duplicate).
    Opaque(VariantType, Option<UtilityCallableId>),
}

/// The identity of a utility function referenced as a first-class `Callable` — its name and the
/// scope `Variant::stringify` qualifies it under. Two references to the same utility carry equal
/// ids, which is what makes `{print: 1, print: 2}` a provable duplicate-key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtilityCallableId {
    pub name: String,
    pub scope: UtilityScope,
}

impl UtilityCallableId {
    /// The callable's text form (`GDScriptUtilityCallable::get_as_text`): `@GlobalScope::print`,
    /// `@GDScript::len`. This is the `%s` the duplicate-key diagnostic names.
    pub fn as_text(&self) -> String {
        format!("{}::{}", self.scope.as_str(), self.name)
    }
}

/// Which scope a utility callable's text form is qualified under
/// (`GDScriptUtilityCallable::Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtilityScope {
    /// A `Variant` utility (`print`, `floor`, …) → `@GlobalScope::<name>`.
    GlobalScope,
    /// A GDScript-only utility (`len`, `range`, …) → `@GDScript::<name>`.
    GDScript,
}

impl UtilityScope {
    fn as_str(self) -> &'static str {
        match self {
            UtilityScope::GlobalScope => "@GlobalScope",
            UtilityScope::GDScript => "@GDScript",
        }
    }
}

/// `NodeId` → its folded constant value, plus the constancy bit for the nodes that have no value
/// this table can hold.
#[derive(Debug, Default)]
pub struct FoldTable {
    values: Vec<Option<FoldedValue>>,
    constant: Vec<bool>,
}

impl FoldTable {
    pub fn new(node_count: usize) -> Self {
        FoldTable {
            values: vec![None; node_count],
            constant: vec![false; node_count],
        }
    }

    /// Record a folded value. Every Godot site that sets `reduced_value` also sets `is_constant`
    /// (audited across `gdscript_analyzer.cpp` — the two always travel together), so this marks
    /// the node constant too.
    pub fn set(&mut self, id: NodeId, value: FoldedValue) {
        self.values[id.index()] = Some(value);
        self.constant[id.index()] = true;
    }

    /// Godot's `is_constant = true` where the folded `Variant` has no [`FoldedValue`] to hold it —
    /// a class object (analyzer.cpp:4046), a preloaded resource (:4778), a cross-file constant
    /// whose value the interface extractor could not read.
    pub fn mark_constant(&mut self, id: NodeId) {
        self.constant[id.index()] = true;
    }

    /// The folded value of a node, or `None` if this table holds no value for it. `None` does NOT
    /// mean "not constant" — see [`Self::is_constant`].
    pub fn get(&self, id: NodeId) -> Option<&FoldedValue> {
        self.values[id.index()].as_ref()
    }

    /// Whether this table holds a materialized value for the node. The guard for anything that
    /// dereferences one; a constancy GATE wants [`Self::is_constant`] instead.
    pub fn is_reduced(&self, id: NodeId) -> bool {
        self.values[id.index()].is_some()
    }

    /// Godot's `ExpressionNode::is_constant` — the wider set. [`Self::is_reduced`] implies it.
    pub fn is_constant(&self, id: NodeId) -> bool {
        self.constant[id.index()]
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
