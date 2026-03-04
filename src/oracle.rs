// Mechanism 5: Type oracle — resolves Rust generic API signatures against
// Toylang types by querying TyCtxt directly. Implemented in Step 8.

extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_hir::def::DefKind;
use rustc_middle::ty::{self, GenericArg, TyCtxt};
use rustc_span::def_id::DefId;
use rustc_span::sym;

use crate::toylang::registry::ToylangRegistry;

/// Walk local HIR definitions to find a struct named `name`.
/// Returns the Ty<'tcx> for it (no generic args — Point is not generic).
pub fn find_local_struct_ty<'tcx>(tcx: TyCtxt<'tcx>, name: &str) -> Option<ty::Ty<'tcx>> {
    for local_def_id in tcx.hir_crate_items(()).definitions() {
        let def_id = local_def_id.to_def_id();
        if tcx.def_kind(def_id) == DefKind::Struct {
            if tcx.item_name(def_id).as_str() == name {
                let adt_def = tcx.adt_def(def_id);
                return Some(ty::Ty::new_adt(tcx, adt_def, ty::List::empty()));
            }
        }
    }
    None
}

/// Find a named method in Vec's inherent impls.
/// Vec has many impl blocks; search all of them.
pub fn find_vec_method(tcx: TyCtxt<'_>, method: &str) -> Option<DefId> {
    let vec_def_id = tcx.get_diagnostic_item(sym::Vec)?;
    for &impl_id in tcx.inherent_impls(vec_def_id) {
        for &item_id in tcx.associated_item_def_ids(impl_id) {
            if tcx.item_name(item_id).as_str() == method {
                return Some(item_id);
            }
        }
    }
    None
}

/// Extract the `Global` allocator type from Vec::new's return type.
/// Vec::new() -> Vec<T, Global>; after instantiating with T=point_ty,
/// the return type's Adt args are [Point, Global], so index 1 is Global.
pub fn extract_global_ty<'tcx>(
    tcx: TyCtxt<'tcx>,
    point_ty: ty::Ty<'tcx>,
    new_def_id: DefId,
) -> Option<ty::Ty<'tcx>> {
    let args = tcx.mk_args(&[ty::GenericArg::from(point_ty)]);
    let sig = tcx.fn_sig(new_def_id).instantiate(tcx, args).skip_binder();
    if let ty::TyKind::Adt(_, adt_args) = sig.output().kind() {
        Some(adt_args[1].expect_ty())
    } else {
        None
    }
}

/// Resolve a function's signature with identity substitution for display.
fn method_sig_str(tcx: TyCtxt<'_>, def_id: DefId) -> String {
    let sig = tcx.fn_sig(def_id).instantiate_identity().skip_binder();
    format!("{:?}", sig)
}

/// Public entry point — called from after_analysis when TOYLANG_DUMP_TYPES=1.
pub fn dump_toylang_oracle<'tcx>(tcx: TyCtxt<'tcx>, registry: &ToylangRegistry) {
    for (name, toy_struct) in &registry.structs {
        let fields_str: Vec<String> = toy_struct.fields.iter()
            .map(|f| format!("{}: {:?}", f.name, f.rust_type))
            .collect();
        eprintln!(
            "[oracle] {}: size={} align={} fields=[{}]",
            name,
            toy_struct.size(),
            toy_struct.align(),
            fields_str.join(", ")
        );

        if let Some(ty) = find_local_struct_ty(tcx, name) {
            eprintln!("[oracle]   Ty = {:?}", ty);
        }

        for method in &["new", "push", "len"] {
            if let Some(def_id) = find_vec_method(tcx, method) {
                let sig = method_sig_str(tcx, def_id);
                eprintln!("[oracle]   Vec<{}>::{} -> {}", name, method, sig);
            }
        }
    }
}
