#![allow(unused)]

extern crate rustc_abi;
extern crate rustc_index;
extern crate rustc_middle;

use rustc_abi::{
    AbiAndPrefAlign, Align, BackendRepr, FieldIdx, FieldsShape, LayoutData, Size, VariantIdx,
    Variants,
};
use rustc_middle::ty::layout::{LayoutError, TyAndLayout};
use rustc_middle::ty::{PseudoCanonicalInput, Ty, TyCtxt};
use std::sync::{Arc, OnceLock};
use crate::toylang::registry::{ToylangRegistry, ToyStruct};

// The provider function type for layout_of on nightly-2025-01-15.
type LayoutOfFn = for<'tcx> fn(
    TyCtxt<'tcx>,
    PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, &'tcx LayoutError<'tcx>>;

// Both are global statics so queries executing on Rayon worker threads
// can read them (thread-locals would only be set on the main rustc thread).
static REGISTRY: OnceLock<Arc<ToylangRegistry>> = OnceLock::new();
static DEFAULT_LAYOUT_OF: OnceLock<LayoutOfFn> = OnceLock::new();

pub fn install_registry(r: Arc<ToylangRegistry>) {
    // Ignore the error if already set (idempotent for test runs).
    let _ = REGISTRY.set(r);
}

pub fn save_default(f: LayoutOfFn) {
    let _ = DEFAULT_LAYOUT_OF.set(f);
}

pub fn toy_layout_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    query: PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, &'tcx LayoutError<'tcx>> {
    let ty = query.value;

    // Detect Toylang types by matching against ADT names only.
    // Restrict to TyKind::Adt so we don't accidentally intercept
    // *mut Point, &mut Point, FnDef(..., [Point]), etc.
    let struct_name = REGISTRY.get().and_then(|reg| {
        if let rustc_middle::ty::TyKind::Adt(adt_def, _) = ty.kind() {
            let name = tcx.item_name(adt_def.did()).to_string();
            reg.structs.keys().find(|k| k.as_str() == name).cloned()
        } else {
            None
        }
    });

    if let Some(name) = struct_name {
        eprintln!("[toylang] layout_of intercepted for: {:?}", ty);
        let reg = REGISTRY.get().expect("registry set above");
        return Ok(build_layout(tcx, ty, &reg.structs[&name]));
    }

    // Fall through to rustc's default provider.
    let default = DEFAULT_LAYOUT_OF.get().expect("default layout_of not saved");
    default(tcx, query)
}

fn build_layout<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
    toy: &ToyStruct,
) -> TyAndLayout<'tcx> {
    use rustc_index::IndexVec;

    let size = Size::from_bytes(toy.size());
    let align = Align::from_bytes(toy.align()).unwrap();
    let abi_align = AbiAndPrefAlign::new(align);

    let offsets: IndexVec<FieldIdx, Size> = toy
        .field_offsets()
        .iter()
        .map(|&o| Size::from_bytes(o))
        .collect();

    let memory_index: IndexVec<FieldIdx, u32> = (0..toy.fields.len() as u32)
        .collect();

    let layout_data = LayoutData {
        fields: FieldsShape::Arbitrary { offsets, memory_index },
        variants: Variants::Single {
            index: VariantIdx::from_u32(0),
        },
        backend_repr: BackendRepr::Memory { sized: true },
        largest_niche: None,
        align: abi_align,
        size,
        max_repr_align: None,
        unadjusted_abi_align: align,
        randomization_seed: 0,
    };

    TyAndLayout {
        ty,
        layout: tcx.mk_layout(layout_data),
    }
}
