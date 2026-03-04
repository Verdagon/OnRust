#![allow(unused)]

extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_hir::def_id::LocalDefId;
use rustc_middle::mir::BorrowCheckResult;
use rustc_middle::ty::TyCtxt;
use std::sync::{Arc, OnceLock};
use crate::toylang::registry::ToylangRegistry;

type BorrowckFn = for<'tcx> fn(TyCtxt<'tcx>, LocalDefId) -> &'tcx BorrowCheckResult<'tcx>;

static REGISTRY: OnceLock<Arc<ToylangRegistry>> = OnceLock::new();
static DEFAULT_MIR_BORROWCK: OnceLock<BorrowckFn> = OnceLock::new();

pub fn install_registry(r: Arc<ToylangRegistry>) {
    let _ = REGISTRY.set(r);
}

pub fn save_default(f: BorrowckFn) {
    let _ = DEFAULT_MIR_BORROWCK.set(f);
}

pub fn toy_mir_borrowck<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> &'tcx BorrowCheckResult<'tcx> {
    if is_toylang_item(tcx, def_id) {
        // Skip borrow checking — hand-built MIR bodies won't pass it.
        tcx.arena.alloc(BorrowCheckResult {
            concrete_opaque_types: Default::default(),
            closure_requirements: None,
            used_mut_upvars: Default::default(),
            tainted_by_errors: None,
        })
    } else {
        let default = DEFAULT_MIR_BORROWCK.get().expect("default mir_borrowck not saved");
        default(tcx, def_id)
    }
}

fn is_toylang_item(tcx: TyCtxt<'_>, def_id: LocalDefId) -> bool {
    // Primary check: file extension (for future .toylang files).
    let span = tcx.def_span(def_id);
    let file = tcx.sess.source_map().lookup_source_file(span.lo());
    if file.name.prefer_local().to_string().ends_with(".toylang") {
        return true;
    }
    // Fallback: name-based registry lookup (PoC, until .toylang file loader exists).
    if let Some(name) = tcx.opt_item_name(def_id.to_def_id()) {
        if let Some(registry) = REGISTRY.get() {
            return registry.functions.contains_key(name.as_str());
        }
    }
    false
}
