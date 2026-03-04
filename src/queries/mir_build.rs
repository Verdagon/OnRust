#![allow(unused)]

extern crate rustc_data_structures;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_data_structures::steal::Steal;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::mir::Body;
use rustc_middle::ty::TyCtxt;
use std::sync::{Arc, OnceLock};
use crate::toylang::registry::ToylangRegistry;

type MirBuiltFn = for<'tcx> fn(TyCtxt<'tcx>, LocalDefId) -> &'tcx Steal<Body<'tcx>>;

static REGISTRY: OnceLock<Arc<ToylangRegistry>> = OnceLock::new();
static DEFAULT_MIR_BUILT: OnceLock<MirBuiltFn> = OnceLock::new();

pub fn install_registry(r: Arc<ToylangRegistry>) {
    let _ = REGISTRY.set(r);
}

pub fn save_default(f: MirBuiltFn) {
    let _ = DEFAULT_MIR_BUILT.set(f);
}

pub fn toy_mir_built<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> &'tcx Steal<Body<'tcx>> {
    if let Some(fn_name) = toylang_fn_name(tcx, def_id) {
        eprintln!("[toylang] mir_built intercepted for: {}", fn_name);

        let body = if let Some(registry) = REGISTRY.get() {
            if let Some(toy_fn) = registry.functions.get(&fn_name) {
                if let Some(fn_body) = &toy_fn.body {
                    // AST-driven path: lower the parsed body
                    let param_names: Vec<String> =
                        toy_fn.params.iter().map(|p| p.name.clone()).collect();
                    crate::toylang::lower::build_body(tcx, def_id, &param_names, fn_body)
                } else {
                    // body: None — hardcoded fallback (e.g. get_x)
                    build_hardcoded(tcx, def_id, &fn_name)
                }
            } else {
                build_hardcoded(tcx, def_id, &fn_name)
            }
        } else {
            build_hardcoded(tcx, def_id, &fn_name)
        };

        return tcx.arena.alloc(Steal::new(body));
    }

    let default = DEFAULT_MIR_BUILT.get().expect("default mir_built not saved");
    default(tcx, def_id)
}

fn build_hardcoded<'tcx>(tcx: TyCtxt<'tcx>, def_id: LocalDefId, fn_name: &str) -> Body<'tcx> {
    match fn_name {
        "get_x" => crate::mir_helpers::build_const_i32_body(tcx, def_id, 42),
        name => panic!("[toylang] no body for '{}' and no AST body in registry", name),
    }
}

fn toylang_fn_name(tcx: TyCtxt<'_>, def_id: LocalDefId) -> Option<String> {
    let name = tcx.opt_item_name(def_id.to_def_id())?.to_string();
    REGISTRY.get()?.functions.contains_key(&name).then_some(name)
}
