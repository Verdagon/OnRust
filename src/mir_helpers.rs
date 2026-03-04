#![allow(unused)]

extern crate rustc_abi;
extern crate rustc_hir;
extern crate rustc_index;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_abi::VariantIdx;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_index::IndexVec;
use rustc_middle::mir::{
    AggregateKind, BasicBlock, BasicBlockData, Body, BorrowKind, CallSource, ClearCrossCrate,
    Const, ConstOperand, ConstValue, Local, LocalDecl, MirSource, MutBorrowKind, Operand, Place,
    PlaceElem, Rvalue, SourceInfo, SourceScopeData, START_BLOCK, Statement, StatementKind,
    Terminator, TerminatorKind, UnwindAction,
};
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::ty::{self, GenericArg, Ty, TyCtxt};
use rustc_span::source_map::Spanned;
use rustc_span::DUMMY_SP;

/// Build a trivial MIR body for a zero-argument function that returns a
/// constant i32. Used to verify the mir_built override fires correctly.
///
/// MIR structure:
///   bb0:
///     _0 = const VALUE_i32;
///     return;
pub fn build_const_i32_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    value: i32,
) -> Body<'tcx> {
    let span = tcx.def_span(def_id);
    let source_info = SourceInfo::outermost(span);

    // Local(0) = return place of type i32
    let mut local_decls = IndexVec::new();
    local_decls.push(LocalDecl::new(tcx.types.i32, span));

    // Assign constant to return place
    let assign_stmt = Statement {
        source_info,
        kind: StatementKind::Assign(Box::new((
            Place::from(Local::from_u32(0)), // RETURN_PLACE
            Rvalue::Use(Operand::Constant(Box::new(ConstOperand {
                span,
                user_ty: None,
                const_: Const::Val(
                    ConstValue::Scalar(Scalar::from_i32(value)),
                    tcx.types.i32,
                ),
            }))),
        ))),
    };

    let terminator = Terminator {
        source_info,
        kind: TerminatorKind::Return,
    };

    let mut basic_blocks = IndexVec::new();
    basic_blocks.push(BasicBlockData::new(Some(terminator), false));
    // Append statement to block (BasicBlockData::new sets statements: vec![])
    basic_blocks[START_BLOCK].statements.push(assign_stmt);

    // One source scope is required (OUTERMOST_SOURCE_SCOPE = index 0)
    let source_scopes = IndexVec::from_elem_n(
        SourceScopeData {
            span,
            parent_scope: None,
            inlined: None,
            inlined_parent_scope: None,
            local_data: ClearCrossCrate::Clear,
        },
        1,
    );

    Body::new(
        MirSource::item(def_id.to_def_id()),
        basic_blocks,
        source_scopes,
        local_decls,
        IndexVec::new(), // user_type_annotations
        0,               // arg_count (get_x takes Point arg but we ignore it for PoC)
        vec![],          // var_debug_info
        span,
        None,            // coroutine
        None,            // tainted_by_errors
    )
}

/// Build a MIR body for drop_in_place::<T> that calls __toylang_drop_T(ptr).
///
/// Signature: fn(*mut T) -> ()
/// MIR:
///   bb0: _0 = __toylang_drop_T(copy _1) -> bb1;
///   bb1: return;
pub fn build_drop_call_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    drop_in_place_def_id: DefId,    // DefId of core::ptr::drop_in_place
    ty: Ty<'tcx>,                   // the Toylang type being dropped
    struct_name: &str,
) -> Body<'tcx> {
    let span = if let ty::TyKind::Adt(adt_def, _) = ty.kind() {
        tcx.def_span(adt_def.did())
    } else {
        DUMMY_SP
    };
    let source_info = SourceInfo::outermost(span);

    // Locals: _0 = (), _1 = *mut T
    let mut local_decls = IndexVec::new();
    local_decls.push(LocalDecl::new(tcx.types.unit, span));               // _0: ()
    local_decls.push(LocalDecl::new(Ty::new_mut_ptr(tcx, ty), span));     // _1: *mut T

    // Find __toylang_drop_{struct_name} in the current crate's extern items
    let drop_fn_name = format!("__toylang_drop_{}", struct_name);
    let drop_fn_def_id = find_extern_fn(tcx, &drop_fn_name);

    // Build the call terminator or fall back to a no-op return
    let (bb0_term, num_blocks) = if let Some(fn_def_id) = drop_fn_def_id {
        let fn_ty = tcx.type_of(fn_def_id).instantiate_identity();
        let func = Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::zero_sized(fn_ty),
        }));
        let call_term = Terminator {
            source_info,
            kind: TerminatorKind::Call {
                func,
                args: vec![Spanned {
                    node: Operand::Copy(Place::from(Local::from_u32(1))),
                    span,
                }].into_boxed_slice(),
                destination: Place::from(Local::from_u32(0)),
                target: Some(BasicBlock::from_u32(1)),
                unwind: UnwindAction::Continue,
                call_source: CallSource::Misc,
                fn_span: span,
            },
        };
        (call_term, 2usize)
    } else {
        eprintln!("[toylang] WARNING: {} not found, drop body is a no-op", drop_fn_name);
        (Terminator { source_info, kind: TerminatorKind::Return }, 1usize)
    };

    let mut basic_blocks = IndexVec::new();
    basic_blocks.push(BasicBlockData::new(Some(bb0_term), false));
    if num_blocks == 2 {
        basic_blocks.push(BasicBlockData::new(
            Some(Terminator { source_info, kind: TerminatorKind::Return }),
            false,
        ));
    }

    let source_scopes = IndexVec::from_elem_n(
        SourceScopeData {
            span,
            parent_scope: None,
            inlined: None,
            inlined_parent_scope: None,
            local_data: ClearCrossCrate::Clear,
        },
        1,
    );

    let mut body = Body::new(
        MirSource::from_instance(ty::InstanceKind::DropGlue(drop_in_place_def_id, Some(ty))),
        basic_blocks,
        source_scopes,
        local_decls,
        IndexVec::new(), // user_type_annotations
        1,               // arg_count = 1 (*mut T)
        vec![],          // var_debug_info
        span,
        None,            // coroutine
        None,            // tainted_by_errors
    );
    // required_consts and mentioned_items must be set or the monomorphization
    // collector panics. Our synthetic body has neither.
    body.set_required_consts(vec![]);
    body.set_mentioned_items(vec![]);
    body
}

/// Build a MIR body for `fn make_vec() -> Vec<Point>` that calls
/// Vec::new(), pushes two Points, and returns the Vec.
///
/// Locals: _0=Vec<Point,Global>, _1=Point, _2=&mut Vec, _3=(), _4=Point, _5=&mut Vec, _6=()
pub fn build_make_vec_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    point_ty: ty::Ty<'tcx>,
    new_def_id: DefId,
    push_def_id: DefId,
    global_ty: ty::Ty<'tcx>,
) -> Body<'tcx> {
    let span = tcx.def_span(def_id);
    let source_info = SourceInfo::outermost(span);

    let new_args = tcx.mk_args(&[GenericArg::from(point_ty)]);
    let push_args = tcx.mk_args(&[GenericArg::from(point_ty), GenericArg::from(global_ty)]);

    // Get vec_ty (Vec<Point, Global>) from Vec::new's return type
    let new_sig = tcx.fn_sig(new_def_id).instantiate(tcx, new_args).skip_binder();
    let vec_ty = new_sig.output();

    let vec_mut_ref_ty = Ty::new_mut_ref(tcx, tcx.lifetimes.re_erased, vec_ty);
    let point_adt_def = point_ty.ty_adt_def().unwrap();

    // Locals
    let mut local_decls = IndexVec::new();
    local_decls.push(LocalDecl::new(vec_ty, span));         // _0: Vec<Point, Global>
    local_decls.push(LocalDecl::new(point_ty, span));       // _1: Point
    local_decls.push(LocalDecl::new(vec_mut_ref_ty, span)); // _2: &mut Vec<Point, Global>
    local_decls.push(LocalDecl::new(tcx.types.unit, span)); // _3: ()
    local_decls.push(LocalDecl::new(point_ty, span));       // _4: Point
    local_decls.push(LocalDecl::new(vec_mut_ref_ty, span)); // _5: &mut Vec<Point, Global>
    local_decls.push(LocalDecl::new(tcx.types.unit, span)); // _6: ()

    // Function operands
    let new_func = Operand::Constant(Box::new(ConstOperand {
        span, user_ty: None,
        const_: Const::zero_sized(Ty::new_fn_def(tcx, new_def_id, new_args)),
    }));
    let push_func_ty = Ty::new_fn_def(tcx, push_def_id, push_args);

    // Helper: Point { x, y } aggregate
    let make_point = |x: i32, y: i32| -> Rvalue<'tcx> {
        Rvalue::Aggregate(
            Box::new(AggregateKind::Adt(
                point_adt_def.did(), VariantIdx::from_u32(0), tcx.mk_args(&[]), None, None,
            )),
            IndexVec::from_raw(vec![
                Operand::Constant(Box::new(ConstOperand {
                    span, user_ty: None,
                    const_: Const::Val(ConstValue::Scalar(Scalar::from_i32(x)), tcx.types.i32),
                })),
                Operand::Constant(Box::new(ConstOperand {
                    span, user_ty: None,
                    const_: Const::Val(ConstValue::Scalar(Scalar::from_i32(y)), tcx.types.i32),
                })),
            ]),
        )
    };

    // Helper: &mut _0
    let mut_ref_zero = || Rvalue::Ref(
        tcx.lifetimes.re_erased,
        BorrowKind::Mut { kind: MutBorrowKind::Default },
        Place::from(Local::ZERO),
    );

    // Helper: push call func operand (needs a fresh one each call)
    let push_func = || Operand::Constant(Box::new(ConstOperand {
        span, user_ty: None,
        const_: Const::zero_sized(push_func_ty),
    }));

    // bb0: _0 = Vec::new() -> bb1
    let bb0 = BasicBlockData {
        statements: vec![],
        terminator: Some(Terminator {
            source_info,
            kind: TerminatorKind::Call {
                func: new_func,
                args: [].into(),
                destination: Place::from(Local::ZERO),
                target: Some(BasicBlock::from_u32(1)),
                unwind: UnwindAction::Continue,
                call_source: CallSource::Misc,
                fn_span: span,
            },
        }),
        is_cleanup: false,
    };

    // bb1: Point{1,2}, &mut _0, push(move _2, move _1) -> bb2
    let bb1 = BasicBlockData {
        statements: vec![
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(1)) },
            Statement { source_info, kind: StatementKind::Assign(Box::new((
                Place::from(Local::from_u32(1)), make_point(1, 2),
            )))},
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(2)) },
            Statement { source_info, kind: StatementKind::Assign(Box::new((
                Place::from(Local::from_u32(2)), mut_ref_zero(),
            )))},
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(3)) },
        ],
        terminator: Some(Terminator {
            source_info,
            kind: TerminatorKind::Call {
                func: push_func(),
                args: vec![
                    Spanned { node: Operand::Move(Place::from(Local::from_u32(2))), span },
                    Spanned { node: Operand::Move(Place::from(Local::from_u32(1))), span },
                ].into_boxed_slice(),
                destination: Place::from(Local::from_u32(3)),
                target: Some(BasicBlock::from_u32(2)),
                unwind: UnwindAction::Continue,
                call_source: CallSource::Misc,
                fn_span: span,
            },
        }),
        is_cleanup: false,
    };

    // bb2: cleanup, Point{3,4}, &mut _0, push(move _5, move _4) -> bb3
    let bb2 = BasicBlockData {
        statements: vec![
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(3)) },
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(2)) },
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(1)) },
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(4)) },
            Statement { source_info, kind: StatementKind::Assign(Box::new((
                Place::from(Local::from_u32(4)), make_point(3, 4),
            )))},
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(5)) },
            Statement { source_info, kind: StatementKind::Assign(Box::new((
                Place::from(Local::from_u32(5)), mut_ref_zero(),
            )))},
            Statement { source_info, kind: StatementKind::StorageLive(Local::from_u32(6)) },
        ],
        terminator: Some(Terminator {
            source_info,
            kind: TerminatorKind::Call {
                func: push_func(),
                args: vec![
                    Spanned { node: Operand::Move(Place::from(Local::from_u32(5))), span },
                    Spanned { node: Operand::Move(Place::from(Local::from_u32(4))), span },
                ].into_boxed_slice(),
                destination: Place::from(Local::from_u32(6)),
                target: Some(BasicBlock::from_u32(3)),
                unwind: UnwindAction::Continue,
                call_source: CallSource::Misc,
                fn_span: span,
            },
        }),
        is_cleanup: false,
    };

    // bb3: cleanup and return
    let bb3 = BasicBlockData {
        statements: vec![
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(6)) },
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(5)) },
            Statement { source_info, kind: StatementKind::StorageDead(Local::from_u32(4)) },
        ],
        terminator: Some(Terminator { source_info, kind: TerminatorKind::Return }),
        is_cleanup: false,
    };

    let mut basic_blocks = IndexVec::new();
    basic_blocks.push(bb0);
    basic_blocks.push(bb1);
    basic_blocks.push(bb2);
    basic_blocks.push(bb3);

    let source_scopes = IndexVec::from_elem_n(
        SourceScopeData {
            span,
            parent_scope: None,
            inlined: None,
            inlined_parent_scope: None,
            local_data: ClearCrossCrate::Clear,
        },
        1,
    );

    // Note: do NOT call set_required_consts/set_mentioned_items here.
    // For mir_built bodies, the mir_promoted pass sets required_consts,
    // and mentioned_items is handled downstream. Only mir_shims need
    // pre-setting (see build_drop_call_body).
    Body::new(
        MirSource::item(def_id.to_def_id()),
        basic_blocks,
        source_scopes,
        local_decls,
        IndexVec::new(),
        0,       // arg_count: make_vec() takes no arguments
        vec![],
        span,
        None,
        None,
    )
}

/// Build a MIR body for `fn vec_len(v: &Vec<Point>) -> usize` that calls
/// Vec::len and returns the result.
///
/// Locals: _0=usize, _1=&Vec<Point,Global>
pub fn build_vec_len_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    point_ty: ty::Ty<'tcx>,
    len_def_id: DefId,
    global_ty: ty::Ty<'tcx>,
) -> Body<'tcx> {
    let span = tcx.def_span(def_id);
    let source_info = SourceInfo::outermost(span);

    let len_args = tcx.mk_args(&[GenericArg::from(point_ty), GenericArg::from(global_ty)]);

    // Get &Vec<Point, Global> from Vec::len's self parameter
    let len_sig = tcx.fn_sig(len_def_id).instantiate(tcx, len_args).skip_binder();
    let vec_imm_ref_ty = len_sig.inputs()[0]; // &Vec<Point, Global>

    // Locals: _0=usize (return), _1=&Vec<Point,Global> (arg)
    let mut local_decls = IndexVec::new();
    local_decls.push(LocalDecl::new(tcx.types.usize, span));    // _0: usize
    local_decls.push(LocalDecl::new(vec_imm_ref_ty, span));     // _1: &Vec<Point, Global>

    let len_func = Operand::Constant(Box::new(ConstOperand {
        span, user_ty: None,
        const_: Const::zero_sized(Ty::new_fn_def(tcx, len_def_id, len_args)),
    }));

    // bb0: _0 = Vec::len(copy _1) -> bb1
    let bb0 = BasicBlockData {
        statements: vec![],
        terminator: Some(Terminator {
            source_info,
            kind: TerminatorKind::Call {
                func: len_func,
                args: vec![
                    Spanned { node: Operand::Copy(Place::from(Local::from_u32(1))), span },
                ].into_boxed_slice(),
                destination: Place::from(Local::ZERO),
                target: Some(BasicBlock::from_u32(1)),
                unwind: UnwindAction::Continue,
                call_source: CallSource::Misc,
                fn_span: span,
            },
        }),
        is_cleanup: false,
    };

    // bb1: return
    let bb1 = BasicBlockData {
        statements: vec![],
        terminator: Some(Terminator { source_info, kind: TerminatorKind::Return }),
        is_cleanup: false,
    };

    let mut basic_blocks = IndexVec::new();
    basic_blocks.push(bb0);
    basic_blocks.push(bb1);

    let source_scopes = IndexVec::from_elem_n(
        SourceScopeData {
            span,
            parent_scope: None,
            inlined: None,
            inlined_parent_scope: None,
            local_data: ClearCrossCrate::Clear,
        },
        1,
    );

    // Note: do NOT call set_required_consts/set_mentioned_items here.
    // These are set by downstream passes (mir_promoted, etc.) for mir_built bodies.
    Body::new(
        MirSource::item(def_id.to_def_id()),
        basic_blocks,
        source_scopes,
        local_decls,
        IndexVec::new(),
        1,       // arg_count: vec_len takes 1 argument (&Vec<Point>)
        vec![],
        span,
        None,
        None,
    )
}

fn find_extern_fn(tcx: TyCtxt<'_>, name: &str) -> Option<DefId> {
    for id in tcx.hir_crate_items(()).foreign_items() {
        let def_id = id.owner_id.def_id.to_def_id();
        if tcx.item_name(def_id).as_str() == name {
            return Some(def_id);
        }
    }
    None
}
