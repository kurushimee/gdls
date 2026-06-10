//! The shared cross-file `extends`-chain resolver.
//!
//! Godot resolves a class's full inheritance chain eagerly in `resolve_class_inheritance`
//! (analyzer.cpp:344-630): each script base loads its depended parser, and the resulting
//! DataType carries the chain's native root through arbitrary script-to-script links
//! (`class_type.native_type = result.native_type`, analyzer.cpp:617-619). gdls resolves lazily
//! over interfaces: this module walks `Interface::extends` links through
//! [`crate::cross_file::CrossFileQuery`] from any [`ScriptRef`], memoized per analysis pass, and
//! reports the native class the chain bottoms out in plus every script link on the way — the one
//! walk behind native-lineage checks (`$`/`@onready`), inherited-member lookup, shadowing
//! warnings, and `is_type_compatible`'s object decomposition.
//!
//! `native_root == None` means **unknown** (unresolvable head, missing interface, or a cycle) —
//! consumers MUST treat that as "stay permissive" (skip node-ness errors, treat members as
//! plausible), never as RefCounted. A cycle pushes no diagnostic here: the offending file gets
//! its own `Cyclic inheritance.` when ITS analysis runs (analyzer.cpp:608-616).

use std::rc::Rc;

use rustc_hash::FxHashSet;

use crate::context::AnalysisContext;
use crate::cross_file::CrossFileQuery;
use crate::data_type::ScriptRef;

/// A resolved `extends` chain starting at (and including) a [`ScriptRef`].
#[derive(Debug)]
pub(crate) struct ResolvedChain {
    /// Every script link, innermost (the start itself) first.
    pub links: Vec<ScriptRef>,
    /// The native class the chain bottoms out in. `Some("RefCounted")` for `Extends::None`
    /// (the implicit base, analyzer.cpp:423-427). `None` = unknown ⇒ permissive.
    pub native_root: Option<String>,
}

/// Walk `start`'s extends chain. Memoized in `AnalysisContext::script_chains` — interfaces are
/// hashmap reads, but `is_type_compatible` runs per argument/assignment, so repeated walks of the
/// same base would multiply.
pub(crate) fn resolve_script_chain(ctx: &AnalysisContext, start: &ScriptRef) -> Rc<ResolvedChain> {
    if let Some(hit) = ctx.script_chains.borrow().get(start) {
        return Rc::clone(hit);
    }
    let chain = Rc::new(walk(ctx, start));
    ctx.script_chains
        .borrow_mut()
        .insert(start.clone(), Rc::clone(&chain));
    chain
}

/// The chain's native root; `None` = unknown ⇒ permissive.
pub(crate) fn chain_native_root(ctx: &AnalysisContext, start: &ScriptRef) -> Option<String> {
    resolve_script_chain(ctx, start).native_root.clone()
}

/// The interface of one chain link (the file's head class, or the named inner class). Takes the
/// query rather than the context so callers holding `&mut AnalysisContext` can keep the returned
/// borrow alive across `push_warning`/`set_type` calls (the `&'x` ties to the analysis-long query
/// lifetime, not to the context borrow).
pub(crate) fn link_interface<'x>(
    xfile: &'x dyn CrossFileQuery,
    link: &ScriptRef,
) -> Option<&'x gd_project::Interface> {
    if link.inner.is_empty() {
        xfile.interface(link.file)
    } else {
        let chain: Vec<&str> = link.inner.iter().map(String::as_str).collect();
        xfile.resolve_inner_chain(link.file, &chain)
    }
}

fn walk(ctx: &AnalysisContext, start: &ScriptRef) -> ResolvedChain {
    let mut links: Vec<ScriptRef> = Vec::new();
    let mut visited: FxHashSet<ScriptRef> = FxHashSet::default();
    let mut cur = start.clone();
    loop {
        if !visited.insert(cur.clone()) {
            // Inheritance cycle — stop with "unknown"; the cycle error belongs to the cyclic
            // file's own analysis, not to whichever consumer walks the chain first.
            return ResolvedChain {
                links,
                native_root: None,
            };
        }
        let Some(iface) = link_interface(ctx.xfile, &cur) else {
            links.push(cur);
            return ResolvedChain {
                links,
                native_root: None,
            };
        };
        // Per-link step mirrors `resolve_class_inheritance`'s extends resolution order
        // (analyzer.cpp:430-528): path → global class_name → native class → the depended file's
        // own inner classes. Outer-scope constants / autoload heads (analyzer.cpp:494-575)
        // aren't interface-visible — those chains end as unknown, never as an error.
        let next = match &iface.extends {
            gd_project::Extends::None => {
                links.push(cur);
                return ResolvedChain {
                    links,
                    native_root: Some("RefCounted".to_owned()),
                };
            }
            gd_project::Extends::Path(p) => match ctx.xfile.resolve_res_path(p) {
                Some(f) => ScriptRef {
                    file: f,
                    inner: Vec::new(),
                },
                None => {
                    links.push(cur);
                    return ResolvedChain {
                        links,
                        native_root: None,
                    };
                }
            },
            gd_project::Extends::Names(names) => {
                let Some(head) = names.first() else {
                    links.push(cur);
                    return ResolvedChain {
                        links,
                        native_root: None,
                    };
                };
                if let Some(f) = ctx.xfile.global_class_file(head) {
                    ScriptRef {
                        file: f,
                        inner: names[1..].to_vec(),
                    }
                } else if ctx.native.class_named(head).is_some() {
                    links.push(cur);
                    // Only the bare `extends NativeClass` form terminates with a known root;
                    // `extends Native.Something` is malformed and stays unknown.
                    let root = (names.len() == 1).then(|| head.clone());
                    return ResolvedChain {
                        links,
                        native_root: root,
                    };
                } else {
                    let chain_names: Vec<&str> = names.iter().map(String::as_str).collect();
                    if ctx
                        .xfile
                        .resolve_inner_chain(cur.file, &chain_names)
                        .is_some()
                    {
                        ScriptRef {
                            file: cur.file,
                            inner: names.clone(),
                        }
                    } else {
                        links.push(cur);
                        return ResolvedChain {
                            links,
                            native_root: None,
                        };
                    }
                }
            }
        };
        links.push(cur);
        cur = next;
    }
}
