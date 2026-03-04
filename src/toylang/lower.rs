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
    Rvalue, SourceInfo, SourceScopeData, Statement, StatementKind, Terminator, TerminatorKind,
    UnwindAction,
};
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::ty::{self, GenericArg, Ty, TyCtxt};
use rustc_span::source_map::Spanned;
use std::collections::HashMap;

use crate::toylang::ast::{Expr, FnBody, Stmt};

// ---------------------------------------------------------------------------
// Lowering state
// ---------------------------------------------------------------------------

struct Lower<'tcx> {
    tcx: TyCtxt<'tcx>,
    span: rustc_span::Span,
    source_info: SourceInfo,
    locals: IndexVec<Local, LocalDecl<'tcx>>,
    local_map: HashMap<String, Local>,
    blocks: IndexVec<BasicBlock, BasicBlockData<'tcx>>,
    current_stmts: Vec<Statement<'tcx>>,
    // Resolved types
    elem_ty: Ty<'tcx>,        // Point
    global_ty: Ty<'tcx>,      // Global allocator
    new_def_id: DefId,
    push_def_id: DefId,
    len_def_id: DefId,
    vec_ty: Ty<'tcx>,         // Vec<Point, Global>
    vec_mut_ref_ty: Ty<'tcx>, // &mut Vec<Point, Global>
}

impl<'tcx> Lower<'tcx> {
    fn alloc_local(&mut self, ty: Ty<'tcx>) -> Local {
        self.locals.push(LocalDecl::new(ty, self.span))
    }

    fn emit(&mut self, stmt: Statement<'tcx>) {
        self.current_stmts.push(stmt);
    }

    /// Finalize the current block with the given terminator.
    /// Returns the index of the next (not-yet-created) block.
    fn finalize_block(&mut self, terminator: Terminator<'tcx>) -> BasicBlock {
        let stmts = std::mem::take(&mut self.current_stmts);
        self.blocks.push(BasicBlockData {
            statements: stmts,
            terminator: Some(terminator),
            is_cleanup: false,
        });
        BasicBlock::from_u32(self.blocks.len() as u32)
    }

    fn storage_live(&self, local: Local) -> Statement<'tcx> {
        Statement { source_info: self.source_info, kind: StatementKind::StorageLive(local) }
    }

    fn storage_dead(&self, local: Local) -> Statement<'tcx> {
        Statement { source_info: self.source_info, kind: StatementKind::StorageDead(local) }
    }

    fn assign_stmt(&self, place: Place<'tcx>, rvalue: Rvalue<'tcx>) -> Statement<'tcx> {
        Statement {
            source_info: self.source_info,
            kind: StatementKind::Assign(Box::new((place, rvalue))),
        }
    }

    fn const_i32(&self, n: i32) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span: self.span,
            user_ty: None,
            const_: Const::Val(
                ConstValue::Scalar(Scalar::from_i32(n)),
                self.tcx.types.i32,
            ),
        }))
    }

    /// Infer the result type of an expression (shallow).
    fn infer_ty(&self, expr: &Expr) -> Ty<'tcx> {
        match expr {
            Expr::IntLit(_) => self.tcx.types.i32,
            Expr::Var(name) => {
                let local = *self.local_map.get(name.as_str())
                    .unwrap_or_else(|| panic!("[toylang] undefined variable '{}'", name));
                self.locals[local].ty
            }
            Expr::StaticCall { ty, method, .. } if ty == "Vec" && method == "new" => self.vec_ty,
            Expr::MethodCall { method, .. } if method == "push" => self.tcx.types.unit,
            Expr::MethodCall { method, .. } if method == "len" => self.tcx.types.usize,
            Expr::StructLit { .. } => self.elem_ty,
            other => panic!("[toylang] infer_ty: unrecognized expression {:?}", other),
        }
    }

    /// Lower an expression, storing the result into `dest`.
    fn lower_expr_into(&mut self, expr: &Expr, dest: Place<'tcx>) {
        match expr {
            Expr::IntLit(n) => {
                let op = self.const_i32(*n as i32);
                let stmt = self.assign_stmt(dest, Rvalue::Use(op));
                self.emit(stmt);
            }

            Expr::Var(name) => {
                let local = *self.local_map.get(name.as_str())
                    .unwrap_or_else(|| panic!("[toylang] undefined variable '{}'", name));
                // Use Move — works for both Copy and non-Copy types in MIR
                let stmt = self.assign_stmt(dest, Rvalue::Use(Operand::Move(Place::from(local))));
                self.emit(stmt);
            }

            Expr::StructLit { name: _, fields } => {
                let tcx = self.tcx;
                let elem_ty = self.elem_ty;

                // Lower each field expr into a temp local
                let mut field_locals: Vec<Local> = Vec::new();
                for (_field_name, field_expr) in fields {
                    let field_ty = self.infer_ty(field_expr);
                    let tmp = self.alloc_local(field_ty);
                    field_locals.push(tmp);
                    let live = self.storage_live(tmp);
                    self.emit(live);
                    self.lower_expr_into(field_expr, Place::from(tmp));
                }

                let adt_def = elem_ty.ty_adt_def().unwrap();
                let operands = IndexVec::from_raw(
                    field_locals.iter()
                        .map(|&l| Operand::Move(Place::from(l)))
                        .collect(),
                );
                let aggregate = Rvalue::Aggregate(
                    Box::new(AggregateKind::Adt(
                        adt_def.did(),
                        VariantIdx::from_u32(0),
                        tcx.mk_args(&[]),
                        None,
                        None,
                    )),
                    operands,
                );
                let stmt = self.assign_stmt(dest, aggregate);
                self.emit(stmt);

                // StorageDead for field temps after the aggregate assignment
                for tmp in field_locals {
                    let dead = self.storage_dead(tmp);
                    self.emit(dead);
                }
            }

            Expr::StaticCall { ty, method, args } if ty == "Vec" && method == "new" => {
                let tcx = self.tcx;
                let span = self.span;
                let new_def_id = self.new_def_id;
                let elem_ty = self.elem_ty;

                let new_args = tcx.mk_args(&[GenericArg::from(elem_ty)]);
                let func = Operand::Constant(Box::new(ConstOperand {
                    span,
                    user_ty: None,
                    const_: Const::zero_sized(Ty::new_fn_def(tcx, new_def_id, new_args)),
                }));

                // Compute next_bb BEFORE finalize (after finalize, blocks.len() increases by 1)
                let next_bb = BasicBlock::from_u32(self.blocks.len() as u32 + 1);
                let term = Terminator {
                    source_info: self.source_info,
                    kind: TerminatorKind::Call {
                        func,
                        args: [].into(),
                        destination: dest,
                        target: Some(next_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: span,
                    },
                };
                self.finalize_block(term);
            }

            Expr::MethodCall { receiver, method, args } if method == "push" => {
                let tcx = self.tcx;
                let span = self.span;
                let push_def_id = self.push_def_id;
                let elem_ty = self.elem_ty;
                let global_ty = self.global_ty;
                let vec_mut_ref_ty = self.vec_mut_ref_ty;

                // Receiver must be a Var pointing to a Vec local
                let recv_local = match receiver.as_ref() {
                    Expr::Var(name) => *self.local_map.get(name.as_str())
                        .unwrap_or_else(|| panic!("[toylang] push receiver '{}' not found", name)),
                    _ => panic!("[toylang] push receiver must be a variable"),
                };

                // &mut Vec temp
                let ref_local = self.alloc_local(vec_mut_ref_ty);
                let live_ref = self.storage_live(ref_local);
                self.emit(live_ref);
                let borrow_rval = Rvalue::Ref(
                    tcx.lifetimes.re_erased,
                    BorrowKind::Mut { kind: MutBorrowKind::Default },
                    Place::from(recv_local),
                );
                let borrow_stmt = self.assign_stmt(Place::from(ref_local), borrow_rval);
                self.emit(borrow_stmt);

                // Argument temp
                let arg = args.first().expect("[toylang] push requires one argument");
                let arg_ty = self.infer_ty(arg);
                let arg_local = self.alloc_local(arg_ty);
                let live_arg = self.storage_live(arg_local);
                self.emit(live_arg);
                self.lower_expr_into(arg, Place::from(arg_local));

                let push_args = tcx.mk_args(&[GenericArg::from(elem_ty), GenericArg::from(global_ty)]);
                let push_func = Operand::Constant(Box::new(ConstOperand {
                    span,
                    user_ty: None,
                    const_: Const::zero_sized(Ty::new_fn_def(tcx, push_def_id, push_args)),
                }));

                let next_bb = BasicBlock::from_u32(self.blocks.len() as u32 + 1);
                let term = Terminator {
                    source_info: self.source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: vec![
                            Spanned { node: Operand::Move(Place::from(ref_local)), span },
                            Spanned { node: Operand::Move(Place::from(arg_local)), span },
                        ].into_boxed_slice(),
                        destination: dest,
                        target: Some(next_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: span,
                    },
                };
                self.finalize_block(term);

                // StorageDead in the successor block (after call returns)
                let dead_arg = self.storage_dead(arg_local);
                self.emit(dead_arg);
                let dead_ref = self.storage_dead(ref_local);
                self.emit(dead_ref);
            }

            Expr::MethodCall { receiver, method, args } if method == "len" => {
                let tcx = self.tcx;
                let span = self.span;
                let len_def_id = self.len_def_id;
                let elem_ty = self.elem_ty;
                let global_ty = self.global_ty;

                // Receiver is already &Vec — just Copy it
                let recv_local = match receiver.as_ref() {
                    Expr::Var(name) => *self.local_map.get(name.as_str())
                        .unwrap_or_else(|| panic!("[toylang] len receiver '{}' not found", name)),
                    _ => panic!("[toylang] len receiver must be a variable"),
                };

                let len_args = tcx.mk_args(&[GenericArg::from(elem_ty), GenericArg::from(global_ty)]);
                let len_func = Operand::Constant(Box::new(ConstOperand {
                    span,
                    user_ty: None,
                    const_: Const::zero_sized(Ty::new_fn_def(tcx, len_def_id, len_args)),
                }));

                let next_bb = BasicBlock::from_u32(self.blocks.len() as u32 + 1);
                let term = Terminator {
                    source_info: self.source_info,
                    kind: TerminatorKind::Call {
                        func: len_func,
                        args: vec![
                            Spanned { node: Operand::Copy(Place::from(recv_local)), span },
                        ].into_boxed_slice(),
                        destination: dest,
                        target: Some(next_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: span,
                    },
                };
                self.finalize_block(term);
            }

            other => panic!("[toylang] lower_expr_into: unhandled expr {:?}", other),
        }
    }

    fn lower_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { name, expr } => {
                let ty = self.infer_ty(expr);
                let local = self.alloc_local(ty);
                self.local_map.insert(name.clone(), local);
                let live = self.storage_live(local);
                self.emit(live);
                self.lower_expr_into(expr, Place::from(local));
            }
            Stmt::ExprStmt(expr) => {
                // Allocate a temp for the (likely unit) result
                let ty = self.infer_ty(expr);
                let tmp = self.alloc_local(ty);
                let live = self.storage_live(tmp);
                self.emit(live);
                self.lower_expr_into(expr, Place::from(tmp));
                // StorageDead goes into whatever block we're in now
                // (the successor block for calls, or the same block for pure exprs)
                let dead = self.storage_dead(tmp);
                self.emit(dead);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Lower a parsed Toylang `FnBody` into a rustc `Body<'tcx>`.
/// `param_names` must be in the same order as the function signature's inputs().
pub fn build_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    param_names: &[String],
    fn_body: &FnBody,
) -> Body<'tcx> {
    let span = tcx.def_span(def_id);
    let source_info = SourceInfo::outermost(span);

    // Resolve types used throughout lowering
    let elem_ty = crate::oracle::find_local_struct_ty(tcx, "Point")
        .expect("[toylang] Point struct not found");
    let new_def_id = crate::oracle::find_vec_method(tcx, "new")
        .expect("[toylang] Vec::new not found");
    let push_def_id = crate::oracle::find_vec_method(tcx, "push")
        .expect("[toylang] Vec::push not found");
    let len_def_id = crate::oracle::find_vec_method(tcx, "len")
        .expect("[toylang] Vec::len not found");
    let global_ty = crate::oracle::extract_global_ty(tcx, elem_ty, new_def_id)
        .expect("[toylang] Global allocator type not found");

    // Vec<Point, Global>
    let new_args = tcx.mk_args(&[GenericArg::from(elem_ty)]);
    let new_sig = tcx.fn_sig(new_def_id).instantiate(tcx, new_args).skip_binder();
    let vec_ty = new_sig.output();

    // &mut Vec<Point, Global>
    let vec_mut_ref_ty = Ty::new_mut_ref(tcx, tcx.lifetimes.re_erased, vec_ty);

    // Function signature: return type and arg types
    let fn_sig = tcx.fn_sig(def_id).instantiate_identity().skip_binder();
    let ret_ty = fn_sig.output();

    // Allocate locals: _0 = return place, _1..N = args
    let mut locals: IndexVec<Local, LocalDecl<'tcx>> = IndexVec::new();
    locals.push(LocalDecl::new(ret_ty, span)); // _0

    let mut local_map: HashMap<String, Local> = HashMap::new();
    for (name, &input_ty) in param_names.iter().zip(fn_sig.inputs().iter()) {
        let local = locals.push(LocalDecl::new(input_ty, span));
        local_map.insert(name.clone(), local);
    }

    let arg_count = fn_sig.inputs().len();

    let mut lower = Lower {
        tcx,
        span,
        source_info,
        locals,
        local_map,
        blocks: IndexVec::new(),
        current_stmts: Vec::new(),
        elem_ty,
        global_ty,
        new_def_id,
        push_def_id,
        len_def_id,
        vec_ty,
        vec_mut_ref_ty,
    };

    // Lower statements
    for stmt in &fn_body.stmts {
        lower.lower_stmt(stmt);
    }

    // Lower trailing return expression
    if let Some(ret_expr) = &fn_body.ret {
        lower.lower_expr_into(ret_expr, Place::return_place());
    }

    // Emit the final Return terminator
    let ret_term = Terminator { source_info, kind: TerminatorKind::Return };
    lower.finalize_block(ret_term);

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

    // Do NOT set required_consts/mentioned_items — mir_promoted handles them for mir_built bodies.
    Body::new(
        MirSource::item(def_id.to_def_id()),
        lower.blocks,
        source_scopes,
        lower.locals,
        IndexVec::new(),
        arg_count,
        vec![],
        span,
        None,
        None,
    )
}
