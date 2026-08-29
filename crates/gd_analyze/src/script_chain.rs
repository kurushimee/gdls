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
pub fn link_interface<'x>(
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

/// One `extends` step from `cur`: either the next script link, or the chain's terminus (the
/// native root, or `None` for unknown). Mirrors `resolve_class_inheritance`'s extends resolution
/// order (analyzer.cpp:430-528): path → global class_name → native class → the depended file's
/// own inner classes. Outer-scope constants / autoload heads (analyzer.cpp:494-575) aren't
/// interface-visible — those chains end as unknown, never as an error.
enum Step {
    Next(ScriptRef),
    Root(Option<String>),
}

fn step(ctx: &AnalysisContext, cur: &ScriptRef) -> Step {
    let Some(iface) = link_interface(ctx.xfile, cur) else {
        return Step::Root(None);
    };
    match &iface.extends {
        gd_project::Extends::None => Step::Root(Some("RefCounted".to_owned())),
        gd_project::Extends::Path(p) => match ctx.xfile.resolve_path_from(cur.file, p) {
            Some(f) => Step::Next(ScriptRef {
                file: f,
                inner: Vec::new(),
            }),
            None => Step::Root(None),
        },
        gd_project::Extends::Names(names) => {
            let Some(head) = names.first() else {
                return Step::Root(None);
            };
            if let Some(f) = ctx.xfile.global_class_file(head) {
                Step::Next(ScriptRef {
                    file: f,
                    inner: names[1..].to_vec(),
                })
            } else if ctx.native.class_named(head).is_some() {
                // Only the bare `extends NativeClass` form terminates with a known root;
                // `extends Native.Something` is malformed and stays unknown.
                Step::Root((names.len() == 1).then(|| head.clone()))
            } else {
                let chain_names: Vec<&str> = names.iter().map(String::as_str).collect();
                if ctx
                    .xfile
                    .resolve_inner_chain(cur.file, &chain_names)
                    .is_some()
                {
                    Step::Next(ScriptRef {
                        file: cur.file,
                        inner: names.clone(),
                    })
                } else {
                    Step::Root(None)
                }
            }
        }
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
        let next = step(ctx, &cur);
        links.push(cur);
        match next {
            Step::Next(n) => cur = n,
            Step::Root(native_root) => return ResolvedChain { links, native_root },
        }
    }
}

/// The full **lexical scope** of `start`, as Godot's `get_class_node_current_scope_classes`
/// builds it (analyzer.cpp:320-344): depth-first over each class's base *and then* its outer
/// class, deduplicated, base prioritized over outer. This is a strictly wider set than
/// [`ResolvedChain::links`], and the two are not interchangeable — Godot keeps them apart too.
/// The inheritance chain is what type compatibility and the native root are computed from, and
/// what a qualified `Base.member` lookup walks; the scope is only ever used to resolve a **bare**
/// identifier, which additionally sees everything its bases' enclosing classes declare.
///
/// `class E extends A.B.D` reaches `A.B`'s constants through `D`'s outer chain even though `A.B`
/// is nowhere in `E`'s inheritance (#314). In-file this is [`crate::resolver::scope_classes`];
/// this is the same walk continued past the file boundary, over interfaces.
pub(crate) fn scope_refs(ctx: &AnalysisContext, start: &ScriptRef) -> Vec<ScriptRef> {
    fn walk_scope(
        ctx: &AnalysisContext,
        node: &ScriptRef,
        out: &mut Vec<ScriptRef>,
        seen: &mut FxHashSet<ScriptRef>,
        depth: usize,
    ) {
        // A malformed interface graph can only ever nest as deep as the inner-class chains the
        // parser produced, but the recursion is over cross-file data, so it gets a hard stop
        // rather than trusting that ("never crash").
        if depth > 64 || !seen.insert(node.clone()) {
            return;
        }
        out.push(node.clone());
        // Base before outer — analyzer.cpp:332-343 prioritizes the base type over the outer
        // class, so a name declared in both resolves to the inherited one.
        if let Step::Next(base) = step(ctx, node) {
            walk_scope(ctx, &base, out, seen, depth + 1);
        }
        if !node.inner.is_empty() {
            let mut outer = node.clone();
            outer.inner.pop();
            walk_scope(ctx, &outer, out, seen, depth + 1);
        }
    }
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();
    walk_scope(ctx, start, &mut out, &mut seen, 0);
    out
}
