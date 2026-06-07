//! String interning. The native DB holds thousands of repeated identifiers (class names, method
//! names, type names); interning them once into a [`Sym`] gives O(1) equality and cheap inheritance
//! walks without re-hashing strings.

use rustc_hash::FxHashMap;

/// An interned string handle. Only minted by an [`Interner`]; comparing two `Sym`s from the *same*
/// interner is a `u32` compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym(u32);

impl Sym {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A string interner: `&str` → [`Sym`] (interning) and `Sym` → `&str` (resolving).
#[derive(Clone, Debug, Default)]
pub struct Interner {
    map: FxHashMap<String, Sym>,
    names: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `s`, returning its handle. Idempotent: the same string always maps to the same `Sym`.
    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let sym = Sym(self.names.len() as u32);
        self.names.push(s.to_owned());
        self.map.insert(s.to_owned(), sym);
        sym
    }

    /// Look up an already-interned string without interning it. Used by queries against a built DB.
    pub fn get(&self, s: &str) -> Option<Sym> {
        self.map.get(s).copied()
    }

    /// Resolve a handle minted by this interner back to its string.
    pub fn resolve(&self, sym: Sym) -> &str {
        &self.names[sym.index()]
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_idempotent() {
        let mut it = Interner::new();
        let a = it.intern("Node");
        let b = it.intern("Node");
        let c = it.intern("Object");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(it.resolve(a), "Node");
        assert_eq!(it.resolve(c), "Object");
        assert_eq!(it.len(), 2);
    }

    #[test]
    fn get_does_not_intern() {
        let mut it = Interner::new();
        it.intern("Node");
        assert!(it.get("Node").is_some());
        assert!(it.get("Missing").is_none());
        assert_eq!(it.len(), 1);
    }
}
