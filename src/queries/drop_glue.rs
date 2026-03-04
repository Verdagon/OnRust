#![allow(unused)]

extern crate rustc_hir;
extern crate rustc_middle;

use rustc_hir::def_id::DefId;
use rustc_middle::mir::Body;
use rustc_middle::ty::{self, TyCtxt};
use std::sync::{Arc, OnceLock};
use crate::toylang::registry::ToylangRegistry;

type MirShimsFn = for<'tcx> fn(TyCtxt<'tcx>, ty::InstanceKind<'tcx>) -> Body<'tcx>;

static REGISTRY: OnceLock<Arc<ToylangRegistry>> = OnceLock::new();
static DEFAULT_MIR_SHIMS: OnceLock<MirShimsFn> = OnceLock::new();

pub fn install_registry(r: Arc<ToylangRegistry>) {
    let _ = REGISTRY.set(r);
}

pub fn save_default(f: MirShimsFn) {
    let _ = DEFAULT_MIR_SHIMS.set(f);
}

pub fn toy_mir_shims<'tcx>(
    tcx: TyCtxt<'tcx>,
    instance: ty::InstanceKind<'tcx>,
) -> Body<'tcx> {
    if let ty::InstanceKind::DropGlue(def_id, Some(ty)) = instance {
        if let Some(struct_name) = toylang_struct_name(tcx, ty) {
            eprintln!("[toylang] mir_shims/DropGlue intercepted for: {}", struct_name);
            return crate::mir_helpers::build_drop_call_body(tcx, def_id, ty, &struct_name);
        }
    }
    let default = DEFAULT_MIR_SHIMS.get().expect("default mir_shims not saved");
    default(tcx, instance)
}

fn toylang_struct_name<'tcx>(tcx: TyCtxt<'tcx>, ty: ty::Ty<'tcx>) -> Option<String> {
    if let ty::TyKind::Adt(adt_def, _) = ty.kind() {
        let name = tcx.item_name(adt_def.did()).to_string();
        REGISTRY.get()?.structs.keys()
            .find(|k| k.as_str() == name)
            .cloned()
    } else {
        None
    }
}
