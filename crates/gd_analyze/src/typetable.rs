//! The per-node resolved-type side table.
//!
//! Godot mutates `node->datatype` in place. gdls keeps `gd_syntax`'s AST engine-free, so resolved
//! types live here instead, in a `Vec` indexed by `NodeId::index()`. `NodeId`s are dense push-indices
//! (`ast.rs`), so a flat `Vec` sized to `ParseTree::len()` is O(1), allocation-cheap, and needs no
//! hashing — strictly better than a `HashMap<NodeId, _>`.

use gd_syntax::ast::NodeId;

use crate::data_type::DataType;

/// `NodeId` → its resolved [`DataType`]. Allocated once per analysis run, sized to the tree.
#[derive(Debug, Default)]
pub struct TypeTable {
    types: Vec<DataType>,
}

impl TypeTable {
    /// A table sized for a tree of `node_count` nodes, every entry defaulting to `Unresolved`.
    pub fn new(node_count: usize) -> Self {
        TypeTable {
            types: vec![DataType::default(); node_count],
        }
    }

    /// Record a node's resolved type. `id` is always in bounds: `NodeId`s are minted only by the
    /// parser and the table is sized to the same tree.
    pub fn set(&mut self, id: NodeId, dt: DataType) {
        self.types[id.index()] = dt;
    }

    /// The resolved type of a node (`Unresolved` if never set).
    pub fn get(&self, id: NodeId) -> &DataType {
        &self.types[id.index()]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut DataType {
        &mut self.types[id.index()]
    }

    /// Iterate every node's type in `NodeId` order. Used by `analyze()`'s finish step to rewrite any
    /// transient in-file `Class` type to a self-referential `Script` ref before the result escapes.
    pub fn iter(&self) -> impl Iterator<Item = &DataType> {
        self.types.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut DataType> {
        self.types.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_type::{DtKind, TypeSource, VariantType};

    #[test]
    fn defaults_to_unresolved_then_records() {
        // NodeId::default() is index 0 — a valid slot in any non-empty table. (External crates
        // can't mint other ids; the analyzer obtains real ones by walking the arena.)
        let id = NodeId::default();
        let mut table = TypeTable::new(4);
        assert_eq!(table.get(id).kind, DtKind::Unresolved);
        table.set(
            id,
            DataType {
                kind: DtKind::Builtin,
                builtin_type: VariantType::Int,
                type_source: TypeSource::AnnotatedExplicit,
                ..Default::default()
            },
        );
        assert_eq!(table.get(id).kind, DtKind::Builtin);
        assert_eq!(table.get(id).builtin_type, VariantType::Int);
    }
}
