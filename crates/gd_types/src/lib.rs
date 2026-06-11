//! `gd_types` — the GDScript type model and the native-class database.
//!
//! The native DB is ingested from Godot's `extension_api.json` (engine classes), with
//! `doc_classes` XML as a static fallback for installed GDExtensions (M2). Type strings are decoded
//! into an unresolved [`TypeRef`]; the analyzer resolves these against the DB and checks
//! assignability in M3 (`docs/02-frontend-port.md`, `docs/03-indexing-freshness.md`).
//!
//! This crate has no `gd_syntax` dependency — a JSON/XML reader needs no AST — so it stays
//! independently testable, like `gd_syntax`.

pub mod api;
pub mod doc_xml;
pub mod intern;
pub mod native_db;
pub mod type_ref;

pub use doc_xml::{parse_class as parse_doc_class, DocXmlError};
pub use intern::{Interner, Sym};
pub use native_db::{
    ApiProvenance, ApiType, BuiltinType, LoadError, Method, NamedConst, NativeClass, NativeDb,
    NativeEnum, NativeMember, Param, Property, Signal, UtilityFn,
};
pub use type_ref::{decode as decode_type, TypeRef};
