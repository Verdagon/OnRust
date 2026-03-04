# Rust Interop via rustc Query Provider: The Broader Architecture

## Scope of This Document

The [Toylang guide](./toylang-rustc-driver-guide.md) covers the proof-of-concept: a toy
language with no generics, a hardcoded struct, and five rustc mechanisms exercised in
isolation. This document covers everything beyond that — the full architecture needed for a
real language that wants first-class interop with Rust's type system and ecosystem.

It addresses:

- How the toy's five mechanisms generalize to a real compiler
- What "your language as a rustc query provider" actually means end-to-end
- How API discovery works without rustdoc hacks
- How type layout works for generics, including mutually recursive cases
- How monomorphization ownership is divided and how the two compilers stay consistent
- How drop glue works across arbitrary nesting depths
- How your language's safety guarantees are preserved at the boundary
- How the build system integrates two compilers into one coherent toolchain
- How to handle nightly API churn without constant firefighting
- The migration path from the toy to production

This document is written for someone who has read and understood the Toylang guide and is
now planning the real implementation.

---

## Part 1: The Mental Model

### 1.1 What "query provider" means

rustc is not a sequential pipeline. It is a **demand-driven computation graph** built on a
query system. When rustc needs to know the layout of `Vec<YourStruct>`, it calls
`tcx.layout_of(Vec<YourStruct>)`. That query calls `tcx.layout_of(YourStruct)`. If your
language has registered a custom provider for `layout_of`, that call lands in your code.

Your provider can then call back into `tcx` freely — to get the layout of fields, to resolve
trait implementations, to get function signatures. The query system memoizes every result and
detects cycles (which represent infinite-size types and are correctly rejected as errors).

The key insight: **your language does not need its own monomorphizer.** Rust's monomorphizer
drives the whole process. When it encounters `YourStruct` as a generic argument, it queries
your provider for whatever it needs. You respond. Rust continues. The two compilers are not
running in parallel or taking turns — your language's logic executes *inside* rustc's query
evaluation, as a first-class participant.

### 1.2 The relationship to `unsafe`

Your language's safety guarantees — whatever they are — are enforced by *your* type checker,
not by Rust's borrow checker. From rustc's perspective, your language's generated MIR is
trusted, the same way `unsafe` blocks are trusted. Rust does not re-verify your language's
invariants. It only verifies its own.

This means:
- Your language's type checker must run to completion *before* MIR is generated
- MIR you inject must be structurally valid (rustc's MIR validator still runs)
- MIR you inject need not satisfy borrow checker rules (you disable borrowck for your items)
- Any safety property your language provides is your responsibility to uphold

The analogy is exact: you are writing a trusted backend, like an `unsafe` block that spans
an entire language.

### 1.3 The compilation order

```
┌─────────────────────────────────────────────────────────────────┐
│  Your language's frontend (runs first, entirely outside rustc)  │
│                                                                  │
│  1. Parse source files                                           │
│  2. Type check (your language's rules, fully)                    │
│  3. Produce generic IR (pre-monomorphization)                    │
│     - One IR body per function, parameterized over type vars     │
│  4. Compute layout formulas for your generic types               │
│     - "MyStruct<T>.size = sizeof(T) + 4, padded to align(T)"    │
│  5. Register everything in a ToylangRegistry                     │
└────────────────────────┬────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────┐
│  rustc session (your language embedded as query providers)       │
│                                                                  │
│  6. rustc starts with Config::override_queries installed         │
│  7. rustc parses and type-checks Rust source files normally      │
│  8. rustc's monomorphizer begins traversing the call graph       │
│     - Encounters YourStruct as a generic arg                     │
│     - Calls tcx.layout_of(YourStruct<int>)                       │
│     → Your layout provider computes it, calling tcx as needed    │
│     - Calls tcx.optimized_mir(your_fn<int>)                      │
│     → Your MIR provider instantiates your generic IR body        │
│     - Calls drop_in_place::<YourStruct<int>>()                   │
│     → Your drop provider returns a MIR body with your destructor │
│  9. rustc codegens everything into a single LLVM module          │
│  10. Link, producing a final binary                              │
└─────────────────────────────────────────────────────────────────┘
```

Steps 1–5 are your compiler. Steps 6–10 are rustc, with your code called from within.

---

## Part 2: API Discovery — Replacing the Rustdoc Approach

### 2.1 The problem with rustdoc JSON

The blog posts established an approach based on invoking `cargo rustdoc --output-format=json`
and parsing the output with the `rustdoc_types` crate. This works for simple cases but has
several hard limits:

- Rustdoc JSON format is unstable and changes between nightly releases
- Rustdoc organizes information for documentation generation, not for compiler use — some
  information is missing or structured inconveniently
- Overload resolution and generics must be reimplemented by hand (3,200+ lines of fragile
  code, as documented in the blog posts)
- Running `cargo rustdoc` as a subprocess is slow and adds build latency

The `rustc_driver` approach replaces all of this with direct `TyCtxt` queries.

### 2.2 The type oracle binary

The right architecture is a standalone `your-lang-oracle` binary — a `rustc_driver` that
takes a crate, a type path, a method name, and optional generic argument types, and outputs
a JSON description of the resolved signature, parameter types, sizes, and alignments.

```bash
# Query: what is Vec<MyStruct>::push's signature, given MyStruct is 8 bytes, align 4?
your-lang-oracle \
  --crate std \
  --type "std::vec::Vec" \
  --method "push" \
  --type-arg "MyStruct:size=8:align=4" \
  --output json
```

Output:
```json
{
  "resolved_fn": "alloc::vec::Vec::<MyStruct>::push",
  "params": [
    { "name": "self", "type": "&mut Vec<MyStruct>", "size": 8, "align": 8 },
    { "name": "value", "type": "MyStruct", "size": 8, "align": 4 }
  ],
  "return": { "type": "()", "size": 0, "align": 1 },
  "trait_bounds_satisfied": true
}
```

### 2.3 How to implement the oracle using TyCtxt

The oracle is a `rustc_driver` binary with a `Callbacks::after_analysis` hook. Inside
`after_analysis`, it has access to a fully-initialized `TyCtxt` with all Rust crates loaded.

**Finding a type by path:**

```rust
// For well-known std types, use diagnostic items
let vec_did = tcx.get_diagnostic_item(rustc_span::sym::Vec)?;

// For arbitrary types, walk the crate graph
fn find_def_id_by_path(tcx: TyCtxt<'_>, path: &[&str]) -> Option<DefId> {
    for krate in tcx.crates(()) {
        for item in tcx.module_children(krate.as_def_id()) {
            // walk the item tree matching path segments
        }
    }
    None
}
```

**Resolving a method with generic arguments:**

```rust
fn resolve_method<'tcx>(
    tcx: TyCtxt<'tcx>,
    type_def_id: DefId,
    method_name: &str,
    type_args: &[Ty<'tcx>],   // concrete types for the generic params
) -> Option<(DefId, FnSig<'tcx>)> {
    let method_did = tcx
        .inherent_impls(type_def_id)
        .iter()
        .flat_map(|&impl_id| tcx.associated_item_def_ids(impl_id))
        .find(|&&did| tcx.item_name(did).as_str() == method_name)?;

    // Substitute the type args into the generic signature
    let args = tcx.mk_args_trait(
        tcx.mk_ty_from_kind(TyKind::Adt(tcx.adt_def(type_def_id), tcx.mk_args(
            &type_args.iter().map(|&t| GenericArg::from(t)).collect::<Vec<_>>()
        ))),
        type_args.iter().map(|&t| GenericArg::from(t)),
    );

    let sig = tcx.fn_sig(method_did).instantiate(tcx, args);
    let sig = tcx.normalize_erasing_regions(ParamEnv::reveal_all(), sig.skip_binder());
    Some((method_did, sig))
}
```

**Trait method resolution (for overloaded methods via traits):**

The UFCS syntax `<OsString as From<&str>>::from` is the correct way to select a specific
trait implementation. In TyCtxt terms:

```rust
// Find the impl of From<&str> for OsString
fn find_trait_impl<'tcx>(
    tcx: TyCtxt<'tcx>,
    self_ty: Ty<'tcx>,
    trait_did: DefId,
    trait_args: &[GenericArg<'tcx>],
) -> Option<DefId> {
    tcx.trait_impls_of(trait_did)
        .non_blanket_impls()
        .values()
        .flatten()
        .find(|&&impl_did| {
            let impl_self_ty = tcx.type_of(impl_did).skip_binder();
            // Check that this impl is for our self_ty
            // and that the trait args match
            impl_self_ty == self_ty
        })
        .copied()
}
```

This is exact overload resolution — you're asking rustc's type system to do the work, not
reimplementing it.

### 2.4 Caching oracle results

The oracle is invoked during your language's build process, before the main compilation
starts. Cache results aggressively — the results are deterministic for a given (crate version,
type, method, type args) tuple. Store the cache in the build directory keyed by a hash of
the inputs.

For standard library types, the oracle results can be cached indefinitely within a nightly
version pin. They change only when the Rust version changes. Use the nightly date as part of
the cache key.

### 2.5 Distinguishing method sources

A type's methods come from multiple sources that need different handling:

| Source | Example | How to find |
|--------|---------|-------------|
| Inherent impl | `Vec::push` | `tcx.inherent_impls(type_did)` |
| Trait impl in same crate | `impl Display for MyType` | `tcx.trait_impls_of(trait_did)` |
| Trait impl in external crate | `impl Iterator for std::vec::IntoIter` | `tcx.all_impls(trait_did)` |
| Blanket impl | `impl<T: Clone> Clone for Vec<T>` | Requires predicate checking |
| Auto trait | `Send`, `Sync` | `tcx.is_auto_trait(trait_did)` |

Your oracle needs to handle all of these to give users a complete picture of what methods are
available on a type.

---

## Part 3: Type Layout for Generics

### 3.1 The shallow case (non-generic types)

For a non-generic type like `struct Point { x: i32, y: i32 }`, the layout is computed once
and cached by the query system. Your `layout_of` provider runs once and returns the same
`LayoutS` every time.

Implementation is straightforward: compute offsets field by field, applying alignment padding,
exactly as described in the Toylang guide.

### 3.2 The generic case — your language's generic types

When your language has generics (`struct MyVec<T> { ptr: *mut T, len: usize, cap: usize }`),
the layout depends on the type argument `T`. The `layout_of` query is called with a specific
instantiation — `layout_of(MyVec<i32>)`, `layout_of(MyVec<Point>)`, etc.

Your provider receives a `Ty<'tcx>` which is already a specific instantiation. Extract the
type arguments:

```rust
fn toy_layout_of<'tcx>(tcx: TyCtxt<'tcx>, query: ParamEnvAnd<'tcx, Ty<'tcx>>) -> ... {
    let ty = query.value;

    if let TyKind::Adt(adt_def, args) = ty.kind() {
        if is_your_lang_type(adt_def.did()) {
            // args contains the concrete type arguments, e.g. [i32] for MyVec<i32>
            let type_arg: Ty<'tcx> = args.type_at(0);

            // Get the layout of the type argument by calling back into tcx
            let arg_layout = tcx.layout_of(query.param_env.and(type_arg))?;

            // Now compute your struct's layout using arg_layout.size, arg_layout.align
            let ptr_size   = tcx.data_layout().pointer_size;
            let ptr_align  = tcx.data_layout().pointer_align.abi;
            let usize_size = tcx.data_layout().pointer_size;  // same as pointer on most targets

            return Ok(build_myvec_layout(tcx, ty, ptr_size, arg_layout));
        }
    }
    DEFAULT_LAYOUT_OF(tcx, query)
}
```

The call to `tcx.layout_of(arg_layout)` is the key: it asks rustc to compute the layout of
the type argument, which may itself be a Rust type, a type from your language, or another
generic instantiation. The query system handles all of this recursively.

### 3.3 The mutually recursive case

This is the case that breaks every simpler architecture:

```rust
// In Rust
struct RustOuter<T> { inner: YourInner<T> }

// In your language  
struct YourInner<T> { field: RustField<T> }
```

When rustc computes `layout_of(RustOuter<i32>)`:

1. rustc needs `layout_of(YourInner<i32>)`
2. Your provider is called with `YourInner<i32>`
3. Your provider needs `layout_of(RustField<i32>)` — calls `tcx.layout_of(RustField<i32>)`
4. rustc computes `layout_of(RustField<i32>)` normally
5. Returns to your provider, which completes `layout_of(YourInner<i32>)`
6. Returns to rustc, which completes `layout_of(RustOuter<i32>)`

**This just works.** The query system's memoization ensures step 4 runs at most once. The
re-entrant call in step 3 (`tcx.layout_of` called from within a `layout_of` provider) is
allowed — rustc's query system is designed for this. The only forbidden case is a true
cycle, which would represent an infinite-size type and is correctly reported as an error.

The critical requirement: your provider must call `tcx.layout_of` for any field whose size
comes from a Rust type, rather than hardcoding sizes. Only `tcx` knows the target-specific
layout of Rust types (pointer size varies by target, struct padding rules vary, etc.).

### 3.4 Target-specific layout concerns

Your language's layout computation must use `tcx.data_layout()` for target-specific sizes,
not hardcoded constants. Key fields:

```rust
let dl = tcx.data_layout();
dl.pointer_size          // usize/isize/pointer width (4 on 32-bit, 8 on 64-bit)
dl.pointer_align.abi     // pointer alignment
dl.i32_align.abi         // i32 alignment (usually 4, but not always)
dl.f64_align.abi         // f64 alignment (varies by target)
dl.aggregate_align.abi   // minimum alignment for aggregate types
```

Never write `size = 8` for a pointer — write `size = dl.pointer_size`. Your language needs
to produce correct code for every rustc target, including 32-bit embedded targets.

### 3.5 Niche optimization

rustc performs niche optimization — it stores enum discriminants in the "niche" of fields
that have unused bit patterns (e.g., `Option<&T>` has the same size as `&T` because null
is the `None` discriminant). If your language has enum-like types, you should populate the
`largest_niche` field of `LayoutS` correctly. If you always set `largest_niche: None`, your
types will never participate in niche optimization, which is safe but suboptimal.

For a first implementation, always use `largest_niche: None`. Add niche support later.

---

## Part 4: MIR Generation

### 4.1 The two-phase MIR strategy

Your language's MIR generation has two phases that must be clearly separated:

**Phase 1 — Generic MIR (your frontend's output):**
Each function produces one MIR body parameterized over its type variables. This is analogous
to what rustc stores for generic Rust functions — the body uses `TyKind::Param` for type
arguments, and `layout_of` calls within it are deferred.

**Phase 2 — Monomorphized MIR (on-demand from the query provider):**
When rustc's monomorphizer needs `your_fn<i32>`, it calls your `mir_built` (or
`optimized_mir`) provider with the specific generic arguments. Your provider takes the
generic body from Phase 1 and substitutes the concrete type arguments, producing a fully
concrete MIR body. This body uses `TyKind::Adt`, `TyKind::Int`, etc. — no `TyKind::Param`.

The substitution is not trivial if done manually, but you can use rustc's own substitution
machinery:

```rust
fn instantiate_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    generic_body: &Body<'tcx>,
    args: GenericArgsRef<'tcx>,
) -> Body<'tcx> {
    // rustc provides EarlyBinder::instantiate_identity and
    // rustc_middle::mir::utils::replace_ty for this purpose
    let mut body = generic_body.clone();
    // Apply substitution to every Ty<'tcx> in the body
    // This requires implementing TypeFoldable or using rustc's own folder
    body
}
```

Alternatively, generate concrete MIR directly in the provider — for simple type systems,
this avoids the need for generic MIR entirely and simplifies the implementation.

### 4.2 MIR body structure — full requirements

A valid MIR body requires more than basic blocks and statements. These fields must all be
correctly populated:

**`source_scopes`:** At minimum one scope (`SourceScope(0)`) with:
```rust
SourceScopeData {
    span: fn_def_span,
    parent_scope: None,
    inlined: None,
    inlined_parent_scope: None,
    local_data: ClearCrossCrate::Clear,
}
```
Every `SourceInfo` in statements and terminators references a scope index. Using index 0 for
everything is valid and sufficient for a first implementation.

**`var_debug_info`:** Empty vec is fine for now. Populate this later for debugger support.
Without it, your language's variables won't be visible in `gdb`/`lldb`, but the code will
still compile and run correctly.

**`local_decls`:** `Local(0)` must have the exact type matching the function's return type
as declared in the function signature (`tcx.fn_sig(def_id).output().skip_binder()`). Every
other local must have an explicit type — `TyKind::Error` locals cause ICEs in codegen.

**`arg_count`:** Must exactly match the number of parameters in the function signature.
`Local(1)` through `Local(arg_count)` correspond to the parameters in order.

**`StorageLive`/`StorageDead` pairs:** Every local except `_0` and arguments must have:
```rust
// Before first use of local N:
StatementKind::StorageLive(Local::from_u32(N))

// After last use of local N:
StatementKind::StorageDead(Local::from_u32(N))
```
Without these, the MIR validator will reject the body. The storage markers tell the borrow
checker (and memory allocators) when a local's stack slot is live.

**`span` field of Body:** Should be the span of the function definition in your language's
source. Use a span derived from your source file — rustc uses this for error attribution and
debug info line numbers. Creating spans for your language's source files requires registering
them with the `SourceMap`:

```rust
let source_file = tcx.sess.source_map().load_file(
    Path::new("myfile.yourlang")
).expect("file not found");
let span = source_file.start_pos..source_file.end_pos;
```

### 4.3 Calling Rust functions from your language's MIR

The `TerminatorKind::Call` is the primary way your language calls Rust functions. The full
form:

```rust
TerminatorKind::Call {
    // The function to call. For a known Rust function:
    func: Operand::function_handle(
        tcx,
        callee_def_id,      // DefId of the Rust function
        generic_args,       // GenericArgsRef for any type params
        call_span,
    ),

    // Arguments. Each is an Operand — Move, Copy, or Constant.
    // The types must match the callee's parameter types exactly.
    args: vec![
        Spanned { node: Operand::Move(vec_place), span: call_span },
        Spanned { node: Operand::Move(elem_place), span: call_span },
    ],

    // Where to write the return value. Must match callee's return type.
    destination: return_place,

    // Where to go after the call returns normally.
    target: Some(next_bb),

    // What to do if the call panics.
    unwind: UnwindAction::Continue,

    call_source: CallSource::Normal,
    fn_span: call_span,
}
```

**Getting the `DefId` for a Rust function:** Use the oracle or the `TyCtxt` query directly:

```rust
// For Vec::push specifically:
let push_did = tcx.inherent_impls(vec_did)
    .iter()
    .flat_map(|&impl_id| tcx.associated_item_def_ids(impl_id))
    .find(|&&did| tcx.item_name(did).as_str() == "push")
    .expect("Vec::push not found");
```

**Type checking your calls:** rustc's MIR validator will check that the argument types match
the callee signature. If they don't, you get a validation error. Always call `tcx.fn_sig`
and verify the types before constructing the `Call` terminator.

### 4.4 Calling your language's functions from Rust MIR

This direction also needs to work — a Rust function that calls a function defined in your
language. Because your language's functions appear in the same `TyCtxt` (they're registered
via `mir_built` override), they already have `DefId`s and Rust can call them via normal
`Call` terminators. No special handling needed on this side.

The only requirement: your language's exported functions must have `extern "C"` or another
stable ABI if they're called across the FFI boundary (e.g., from a separately compiled Rust
crate). For calls within the same compilation unit, the Rust ABI is fine.

### 4.5 Unwind handling

Every `Call` terminator has an `unwind` field. For the first implementation, use
`UnwindAction::Continue` everywhere — this means panics from Rust code will unwind through
your language's frames without any cleanup. This is incorrect in general (your language's
destructors won't run if Rust panics mid-call) but is safe enough to start with.

Full unwind support requires:
1. Generating landing pad basic blocks (`is_cleanup: true`) for each frame that has live
   values needing cleanup
2. Using `UnwindAction::Cleanup(cleanup_bb)` in Call terminators
3. Generating drop calls in cleanup blocks

This is a significant amount of work and should be treated as a separate milestone.

### 4.6 Constants and static values

Constant values in MIR use `ConstOperand`:

```rust
// An integer constant
Operand::Constant(Box::new(ConstOperand {
    span,
    user_ty: None,
    const_: Const::Val(
        ConstValue::Scalar(Scalar::from_u64(42)),
        tcx.types.u64,
    ),
}))

// A zero-sized type constant (e.g., a function item)
Operand::Constant(Box::new(ConstOperand {
    span,
    user_ty: None,
    const_: Const::zero_sized(tcx.mk_fn_def(fn_did, generic_args)),
}))
```

For string literals, use `ConstValue::Slice` with the string bytes interned in the `TyCtxt`.

---

## Part 5: Monomorphization Ownership

### 5.1 The division of labor

rustc's monomorphizer (in `rustc_monomorphize`) traverses the MIR reachability graph
starting from all `#[no_mangle]` and `pub extern` functions, following every `Call`
terminator and every mention of a concrete type. It collects all `MonoItem`s — concrete
function bodies and static values — that need code generation.

When it encounters a call to one of your language's functions with concrete type arguments,
it:
1. Requests `tcx.optimized_mir(your_fn_instance)` via the query system
2. Your provider returns the instantiated body
3. The monomorphizer adds `your_fn<ConcreteType>` to its worklist
4. It recurses into the body, following any Rust calls it finds there

**Your language does not need to maintain a separate list of "things to monomorphize."**
The monomorphizer discovers everything by following calls. You just need to respond correctly
when asked for a specific instantiation.

### 5.2 Ensuring all instantiations are reachable

The monomorphizer only visits items reachable from root items. If a function in your language
is only called from your language, and not from any `pub extern` Rust function, the
monomorphizer may not visit it.

The solution: every entry point in your language that needs to be accessible from outside
must have a corresponding `pub extern "C"` wrapper function, either:
- Written explicitly in Rust glue code
- Generated by your compiler and registered with rustc via `mir_built`

Alternatively, use `#[rustc_std_internal_symbol]` or a similar attribute to force
monomorphization of items that aren't otherwise reachable. In practice, exposing everything
through `pub extern "C"` wrappers is simpler and more portable.

### 5.3 Duplicate monomorphization

If both your language's compiler and rustc attempt to emit machine code for the same
function (e.g., because your language has its own codegen path that it runs before handing
off to rustc), you'll get duplicate symbol linker errors.

The solution is simple: **your language must not emit any machine code directly.** All
codegen goes through rustc. Your language's compiler produces MIR. rustc produces machine
code. There is no other codegen path.

This is the fundamental difference between "your language as a rustc query provider" and
"your language as a separate compiler that links with Rust." In the query provider model,
you produce zero object files. rustc produces all of them.

### 5.4 Incremental compilation

rustc's incremental compilation system (`rustc -C incremental=dir`) caches query results
between compilations. When a source file changes, only the affected queries are recomputed.

Your query providers participate in this system automatically — if `layout_of(YourStruct)`
returns the same result as last time (because `YourStruct` didn't change), rustc won't
recompute it. The cache key is based on the query inputs.

However, rustc has no way to know that a change to your language's source file invalidates
a specific `layout_of` result. You must tell it, by adjusting the `DefId` or `Ty` used as
the query key to incorporate a hash of the relevant source.

For a first implementation, disable incremental compilation for your language's outputs by
setting an always-changing hash in the query inputs. Add proper incremental support later.

---

## Part 6: Drop Glue — The Full Picture

### 6.1 The drop chain

When a value of type `T` is dropped, rustc executes this logic:

1. If `T` has an explicit `Drop` impl, call `<T as Drop>::drop(&mut self)`
2. Then drop each field in declaration order, recursively

For `Vec<YourStruct>`, dropping works like this:

```
drop(Vec<YourStruct>)
  → Vec's Drop impl runs (frees the heap allocation)
  → before freeing, drops each element: drop(YourStruct)
    → your language's destructor runs (Step 1 for YourStruct)
    → drop each field of YourStruct (Step 2)
      → if a field is of Rust type, rustc's drop glue runs for it
```

This entire chain must be correctly wired up.

### 6.2 Providing drop glue via `instance_mir`

The `mir_built` query handles user-defined function bodies. Drop glue for synthetic items
(including `drop_in_place` shims) comes through `instance_mir`:

```rust
providers.instance_mir = |tcx, instance_kind| {
    match instance_kind {
        InstanceKind::DropGlue(_, Some(ty)) if is_your_lang_type(tcx, ty) => {
            build_drop_body(tcx, ty)
        }
        _ => DEFAULT_INSTANCE_MIR(tcx, instance_kind)
    }
};
```

### 6.3 Building the drop body

For a type with no destructor and no fields needing drop, the body is a single `Return`:

```
fn drop_in_place::<YourStruct>(ptr: *mut YourStruct) {
    return;
}
```

For a type with a destructor:

```
fn drop_in_place::<YourStruct>(ptr: *mut YourStruct) {
  bb0:
    your_destructor(ptr as *mut ());   // call your lang's destructor
    goto bb1;

  bb1:
    drop_in_place::<FieldType>(&mut (*ptr).field);  // drop each field
    goto bb2;

  bb2:
    return;
}
```

For generic types (`YourStruct<T>` where `T` might need drop):

```
fn drop_in_place::<YourStruct<T>>(ptr: *mut YourStruct<T>) {
  bb0:
    your_destructor_generic(ptr as *mut (), drop_in_place::<T> as fn(*mut T));
    // ^ pass a pointer to T's drop function so your destructor can call it
    goto bb1;

  bb1:
    return;
}
```

Passing function pointers to drop functions is how rustc itself implements
generic drop glue — it's the same mechanism used by `Box<dyn Any>` internally.

### 6.4 The `NeedsDrop` query

`tcx.needs_drop(ty, param_env)` returns whether a type needs drop glue. rustc uses this to
optimize away drop calls for types that don't need them (e.g., `i32`). Your `layout_of`
provider should also inform `needs_drop` for your types:

```rust
providers.needs_drop_raw = |tcx, query| {
    let ty = query.value;
    if is_your_lang_type_with_destructor(tcx, ty) {
        return true;
    }
    // Check fields too
    for field_ty in your_lang_field_types(tcx, ty) {
        if tcx.needs_drop_raw(query.param_env.and(field_ty)) {
            return true;
        }
    }
    false
};
```

Without this, rustc may elide drop calls for your types even when they have destructors.

### 6.5 Panic safety during drop

If your language's destructor panics (or if a Rust function called from your destructor
panics), the behavior depends on how you set up unwind handling in your drop MIR body. For
a first implementation, `UnwindAction::Terminate` in the drop body is safest — it terminates
the process rather than attempting to unwind through a partially-dropped value, which mirrors
Rust's behavior when a `Drop` impl itself panics.

---

## Part 7: Your Language's Safety Guarantees at the Boundary

### 7.1 What rustc checks vs. what you check

With borrowck disabled for your language's items, rustc performs the following checks on
your MIR:

- **MIR structural validity** (the MIR validator): basic block structure, local types,
  terminator correctness. You cannot disable this.
- **Type checking of MIR operations**: assignment types must match, call argument types
  must match callee signatures. You cannot disable this.
- **Codegen correctness**: rustc will generate correct machine code for the MIR you provide,
  including correct ABI handling for calls.

rustc does **not** check:

- Whether your language's ownership/lifetime rules are satisfied
- Whether your language's linear type invariants are upheld
- Whether your language's safety properties hold at the boundary

These are your responsibility. The guarantee is: if your language's type checker approves a
program, the MIR you generate must correctly implement the semantics your type checker
verified.

### 7.2 The boundary invariant

The key invariant to maintain is: **Rust never observes a partially-initialized or
use-after-free'd value of your language's types.** This means:

- When you pass a value from your language to Rust (e.g., push it into `Vec`), the value
  must be fully initialized and the memory at the correct layout rustc expects.
- When Rust drops a value of your language's type, your destructor will be called exactly
  once, at a time when the value is still fully initialized.
- When your language receives a reference from Rust, the value behind the reference is
  live for at least as long as you use the reference.

These are the same invariants that `unsafe` Rust code must maintain. Your language's type
checker should verify them on your language's side; the drop glue and layout machinery
ensures Rust holds up its end.

### 7.3 Linear types at the boundary

If your language has linear types (values that must be used exactly once), the boundary
introduces a risk: when you pass a linear value to Rust (e.g., into `Vec`), Rust takes
ownership. If Rust drops the `Vec` without calling your language's destructor through the
correct drop glue chain, you have a leak. Conversely, if Rust calls `drop_in_place` twice
(a bug in your drop glue), you have a double-destroy.

The drop glue implementation described in Part 6 handles the destroy side correctly. For
the tracking side (ensuring linear values aren't forgotten), your language's type checker
must verify that every value passed to Rust is consumed by a path that guarantees eventual
destruction — e.g., that `Vec<YourLinear>` will eventually be dropped, which triggers the
drop chain.

This is a type-system concern, not a codegen concern. Your type checker must model Rust
types that hold your language's types, tracking that they will be dropped.

---

## Part 8: The Build System

### 8.1 The two-compiler build flow

The build system must orchestrate two compilers in the correct order:

```
Source files
  ├── *.yourlang  → your frontend compiler → ToylangRegistry + generic MIR
  └── *.rs        ─────────────────────────────────────────────────────────┐
                                                                           ▼
                                               rustc_driver (your binary) ──→ binary
                                               (embeds registry, provides
                                                queries on demand)
```

In practice, the "two compilers" are:
1. Your frontend binary (parses, type-checks, produces a registry)
2. Your rustc_driver binary (runs the rustc session with your providers)

These can be the same binary with two subcommands, or two separate binaries. The registry
is passed from step 1 to step 2 via a serialized file (JSON, CBOR, or your own format) or
via shared memory if both are in-process.

### 8.2 Cargo integration

The cleanest Cargo integration uses a **build script** (`build.rs`) plus a **custom runner**
or **cargo subcommand**:

**Option A: Build script invokes your compiler, outputs files consumed by a custom Cargo
runner that replaces `rustc`:**

```toml
# Cargo.toml
[package]
build = "build.rs"

[build-dependencies]
your-lang-build = { path = "../your-lang-build" }
```

```rust
// build.rs
fn main() {
    // Invoke your language's frontend on all .yourlang files
    let registry = your_lang_build::compile_yourlang_files();
    
    // Write registry to a file in OUT_DIR
    let out_dir = std::env::var("OUT_DIR").unwrap();
    registry.write_to(&format!("{}/toylang_registry.bin", out_dir));
    
    // Tell cargo to rerun if any .yourlang file changes
    for file in glob::glob("src/**/*.yourlang").unwrap() {
        println!("cargo:rerun-if-changed={}", file.unwrap().display());
    }
}
```

**Option B: `RUSTC_WRAPPER` environment variable:**

Cargo supports replacing the `rustc` invocation with a custom binary via `RUSTC_WRAPPER`.
Your binary receives the same arguments `rustc` would receive, plus an additional first
argument (the path to the real `rustc`). This is how tools like `sccache` (a caching
compiler wrapper) work.

```bash
RUSTC_WRAPPER=your-lang-rustc-wrapper cargo build
```

Inside your wrapper, detect whether the current compilation unit contains your language's
items. If it does, run your full driver. If not, exec the real `rustc` directly. This
approach requires no changes to the user's `Cargo.toml`.

**Option C: Cargo's `build.target-dir` + `cargo +your-toolchain`:**

For projects that are all-in on your language, define a custom toolchain that replaces
`rustc` entirely:

```toml
# rust-toolchain.toml (in user's project)
[toolchain]
channel = "nightly-2025-01-15"

# Custom rustc replacement
[toolchain.components]
rustc = "your-lang-rustc"
```

This requires publishing your driver as a rustup component, which is the most involved but
most seamless user experience.

For the near term, Option B (RUSTC_WRAPPER) is the simplest path and the one most existing
tools use.

### 8.3 Registry serialization

The registry produced by your frontend and consumed by the rustc driver needs a
serialization format. Requirements:

- **Deterministic:** same input → same bytes. Needed for build caching.
- **Incremental:** only the changed structs/functions are recomputed when a .yourlang file
  changes.
- **Version-stamped:** include the registry format version and nightly pin date. Reject
  registries from incompatible versions with a clear error.

A simple approach: use `serde` + `bincode` for compact binary, or `serde` + `json` for
debuggability. Stamp with a hash of the nightly toolchain version.

### 8.4 Dependency tracking

When a Toylang type changes:
- `layout_of` results for that type are invalid (and for any type that contains it)
- MIR bodies that use that type are invalid
- Rust crates that depend on the type's size/layout need recompilation

Cargo's dependency tracking doesn't know about your language's types. You must tell it:

```rust
// In build.rs, emit rerun-if-changed for every .yourlang file
for file in find_yourlang_files() {
    println!("cargo:rerun-if-changed={}", file);
}
// Also emit a hash of the registry itself, so Rust files recompile when any type changes
println!("cargo:rerun-if-env-changed=TOYLANG_REGISTRY_HASH");
std::env::set_var("TOYLANG_REGISTRY_HASH", registry_hash);
```

---

## Part 9: Handling Nightly API Churn

### 9.1 What changes, what doesn't

Between nightly versions, the following typically change:

| Component | Stability | Change frequency |
|-----------|-----------|-----------------|
| `Callbacks` trait | Very stable | Rarely |
| `override_queries` mechanism | Very stable | Rarely |
| `TyCtxt` query names | Stable | Rarely |
| `Body`, `BasicBlock`, `Statement` structure | Mostly stable | Occasionally |
| `LayoutS` field names and constructor | Unstable | Frequently |
| `TerminatorKind` variant fields | Unstable | Occasionally |
| `Providers` struct (new fields) | Grows frequently | Always safe to ignore |
| `BorrowCheckResult` fields | Unstable | Occasionally |
| `GenericArgs` API | Somewhat stable | Occasionally |

The `LayoutS` constructor is the most common source of breakage. Expect to fix it on most
monthly updates.

### 9.2 The update process

Define an explicit monthly process:

1. Create a `chore/nightly-YYYY-MM` branch
2. Update `rust-toolchain.toml` date
3. Run `cargo build`
4. For each compilation error:
   - Check the rustc changelog or git blame for what changed
   - Fix the usage
5. Run the full test suite
6. Merge with a commit message: `chore: bump nightly to YYYY-MM-DD`

Keep the update commits clean and atomic. They make it easy to bisect if a nightly
introduces a regression in your code vs. a regression in rustc.

### 9.3 Abstraction layer

Isolate all `rustc_private` API usage behind a thin abstraction layer in your codebase.
Never use `LayoutS`, `BasicBlockData`, or similar types directly in your language's
frontend code. Only your `queries/` module should touch these types.

```
your_lang/
  frontend/     ← zero rustc_private imports
  queries/      ← all rustc_private usage isolated here
  mir_helpers/  ← rustc_private, but only MIR construction utilities
```

When a nightly breaks something, the fix is confined to `queries/` and `mir_helpers/`.
Your frontend code is untouched.

### 9.4 Migration to rustc_public

The `rustc_public` / Stable MIR project (https://github.com/rust-lang/project-stable-mir)
is designing a stable, versioned public API for exactly this use case. Monitor it actively.

When it stabilizes:
- Replace `#![feature(rustc_private)]` with a versioned dependency on `rustc_public`
- Replace `LayoutS` construction with the stable layout API
- Replace `Body::new()` with the stable MIR construction API

The query override mechanism (`Config::override_queries`) is expected to be part of the
stable API. The migration will be significant work but will eliminate nightly coupling.

---

## Part 10: Milestones — From Toy to Production

### Milestone 0: Toylang proof of concept (current)

Covers the Toylang guide. Goal: validate all five mechanisms work.

**Exit criteria:**
- `Vec<ToylangStruct>` compiles and runs
- `drop_in_place` fires correctly
- Type oracle resolves `Vec::push` signature
- Build compiles on a pinned nightly without errors

---

### Milestone 1: Your real language's type system, non-generic types only

Connect your real compiler's type checker output to the query provider infrastructure.

**What to implement:**
- Your language's real parser and type checker (probably already exists)
- Serialized registry format (your types → registry)
- `layout_of` for your language's concrete (non-generic) types, using actual field info
- `mir_built` for simple function bodies (no generics, no closures)
- Build script / RUSTC_WRAPPER integration
- End-to-end test: a real program in your language calling a Rust stdlib function

**Key question to answer at this milestone:**
Does your language's type representation map cleanly to the registry format? Are there
features of your type system that don't fit the "struct with fields" model?

---

### Milestone 2: Generic types (your language's generics over Rust types)

**What to implement:**
- `layout_of` for your language's generic types, calling `tcx.layout_of` for type args
- `mir_built` that instantiates generic MIR bodies with concrete type args
- `needs_drop_raw` override for your generic types
- Test: `YourVec<i32>`, `YourOption<RustStruct>` — your generic types wrapping Rust types

**Key challenge:** Generic MIR body instantiation. Decide whether you produce generic bodies
in Phase 1 and instantiate them in Phase 2, or always generate concrete bodies in the query
provider. The latter is simpler but may be slower for heavily-parameterized types.

---

### Milestone 3: Rust generics over your types

**What to implement:**
- Your types appearing as generic arguments to Rust types: `Vec<YourStruct>`
- Correct `layout_of` so Rust can compute layouts of `Vec<YourStruct>`, `Box<YourStruct>`,
  etc.
- Drop glue chain: `drop(Vec<YourStruct>)` → `drop_in_place::<YourStruct>()` →
  your destructor

**Test:** `std::collections::HashMap<YourKey, YourValue>` — exercises layout, hashing
(requires `Hash` trait impl), equality (requires `Eq` trait impl), and drop.

**Key challenge:** Trait implementations. For `HashMap<YourKey, YourValue>`, `YourKey`
must implement `Hash + Eq`. These implementations need to be registered with rustc so that
trait dispatch works correctly. See Milestone 4.

---

### Milestone 4: Trait implementations

**What to implement:**
- Registering your language's trait implementations with rustc's trait system
- For each `impl SomeTrait for YourType`, providing MIR bodies for all trait methods
- Override `trait_impls_of` or similar queries to make your impls visible

This is the most complex milestone. Trait implementations affect:
- Method resolution (`obj.method()` selecting the right impl)
- Trait object dispatch (`dyn Trait`)
- Blanket impl applicability (does `impl<T: YourTrait> for Vec<T>` apply to your types?)
- Coherence checking (no duplicate impls)

For a first implementation, limit trait support to a specific list of traits your language
explicitly implements: `Clone`, `Copy`, `Debug`, `Display`, `Hash`, `Eq`, `Ord`, `Drop`.

---

### Milestone 5: Mutually recursive generic nesting

The case described in detail earlier:

```rust
// Rust
struct RustOuter<T> { inner: YourInner<T> }

// Your language
struct YourInner<T> { field: RustField<T> }
```

**What to implement:**
- Verify that your `layout_of` provider calls `tcx.layout_of` for Rust-typed fields (should
  already be true from Milestone 2, but needs explicit testing)
- Comprehensive test cases for 3+ levels of nesting
- Cycle detection test: verify that a genuinely cyclic layout produces a clear error

**This milestone should "just work"** if Milestones 2–4 were implemented correctly. The
test suite is the main deliverable — proving that the query system's demand-driven evaluation
handles arbitrary nesting without issues.

---

### Milestone 6: Build system hardening

**What to implement:**
- Proper incremental compilation support (invalidate cached queries when source changes)
- Hermetic build: registry format versioned, nightly version embedded in binary
- `RUSTC_WRAPPER` integration tested in a real Cargo workspace
- CI pipeline that tests on the current pinned nightly and detects churn breakage

---

### Milestone 7: Diagnostics and debugger support

**What to implement:**
- Source spans for your language's items registered with rustc's `SourceMap`
- `var_debug_info` populated in MIR bodies (for gdb/lldb variable names)
- Error attribution: rustc errors that point into your language's source files should show
  the right file name and line number
- Panic messages from Rust code that mention your language's types should use human-readable
  names (requires `tcx.def_path_str` to return sensible names)

---

### Milestone 8: Migration to rustc_public

**Contingent on:** rustc_public stabilization (expected 2025–2026)

**What to implement:**
- Replace all `rustc_private` usage with `rustc_public` equivalents
- Remove `#![feature(rustc_private)]` and nightly requirement
- Test on stable Rust toolchain

---

## Part 11: Reference — Key rustc APIs

This section collects the most important `TyCtxt` queries and types for quick reference.

### Layout queries

```rust
tcx.layout_of(param_env.and(ty))     // LayoutS for a type in a param env
tcx.data_layout()                     // Target-specific layout info
tcx.is_sized(ty, param_env)          // Is this type Sized?
tcx.needs_drop_raw(param_env.and(ty)) // Does this type need drop glue?
```

### Type construction

```rust
tcx.types.i32                         // Ty<'tcx> for i32
tcx.types.bool                        // Ty<'tcx> for bool
tcx.types.unit                        // Ty<'tcx> for ()
tcx.mk_ptr(TypeAndMut { ty, mutbl })  // *mut T or *const T
tcx.mk_ref(region, TypeAndMut { ty, mutbl }) // &T or &mut T
tcx.mk_adt(adt_def, args)             // An ADT type with generic args
tcx.mk_fn_ptr(sig)                    // A function pointer type
```

### Definition lookup

```rust
tcx.def_kind(def_id)                  // What kind of item (Fn, Struct, Trait, etc.)
tcx.item_name(def_id)                 // Name of an item as a Symbol
tcx.def_span(def_id)                  // Source span of a definition
tcx.get_diagnostic_item(sym::Vec)     // DefId of a well-known item by name
tcx.inherent_impls(type_did)          // All inherent impls for a type
tcx.trait_impls_of(trait_did)         // All impls of a trait
tcx.associated_item_def_ids(impl_did) // All items in an impl block
tcx.associated_item(assoc_did)        // Info about an associated item
```

### MIR queries

```rust
tcx.mir_built(local_def_id)           // Raw MIR (before optimization)
tcx.optimized_mir(def_id)             // Optimized MIR (for codegen)
tcx.mir_keys(())                      // All LocalDefIds that have MIR
tcx.instance_mir(instance_kind)       // MIR for a specific instance (incl. shims)
```

### Type inspection

```rust
ty.kind()                             // TyKind — the type's variant
ty.is_primitive()                     // Is it a primitive type?
ty.is_adt()                           // Is it a struct/enum/union?
ty.is_fn()                            // Is it a function type?
if let TyKind::Adt(adt_def, args) = ty.kind() { ... }
if let TyKind::Ref(region, inner_ty, mutbl) = ty.kind() { ... }
```

### Function signatures

```rust
tcx.fn_sig(def_id)                    // Generic signature (EarlyBinder)
tcx.fn_sig(def_id).skip_binder()      // FnSig without binder
tcx.fn_sig(def_id).instantiate(tcx, args) // Monomorphized signature
sig.inputs()                          // Parameter types (slice)
sig.output()                          // Return type
sig.abi                               // Calling convention (Rust, C, etc.)
```

### Trait system

```rust
tcx.trait_def(trait_did)              // TraitDef for a trait
tcx.is_auto_trait(trait_did)          // Is this an auto trait (Send, Sync)?
tcx.impl_trait_ref(impl_did)          // The trait + args this impl implements
tcx.check_impls_are_allowed_to_overlap(def_id, other) // Coherence check
```

### Source map and spans

```rust
tcx.sess.source_map()                 // The SourceMap
source_map.lookup_source_file(pos)    // Which file contains a byte position
source_map.load_file(path)            // Load a source file and get its span range
```

---

## Part 12: Known Unknowns

These are areas where the approach described in this document has open questions that will
require research and experimentation during implementation:

**Cross-crate type registration:** The query provider approach works when your language's
types and the Rust code using them are compiled in the same `rustc` session. When Rust code
in crate B depends on types defined in your language's code compiled in crate A (a separate
earlier session), the types need to be visible to crate B's compilation without re-running
your language's frontend. This requires serializing your type definitions into the `.rmeta`
file that rustc stores for compiled crates. The `rustc_metadata` crate handles `.rmeta`
writing, and hooking into it to add your types is unexplored territory. One workaround:
always compile your language's types and Rust code in the same session. This limits
separate compilation but avoids the `.rmeta` problem.

**Trait coherence with your language's types:** Rust's orphan rule says you can only implement
a trait for a type if you own either the trait or the type. When registering trait impls for
your language's types via the query system, the coherence checker may reject your impl
because it doesn't recognize your type as "local" to the current crate. Whether and how
`override_queries` can bypass this is untested.

**Async and generators:** If your language or any Rust library it calls uses `async fn` or
generators, the MIR representation is significantly more complex (coroutine state machines).
The `MirSource` and `Body` for async functions contain additional fields. Scope this as a
separate milestone if needed.

**LTO (Link-Time Optimization):** When LTO is enabled, rustc serializes MIR into the object
files for cross-crate optimization. MIR bodies injected via query providers are included in
this process. No known issues, but untested with LTO.

**WASM and embedded targets:** The `layout_of` implementation must use `tcx.data_layout()`
for all target-specific sizes. If this is done correctly from the start, cross-compilation
to non-native targets should work. Test early on a 32-bit target (e.g., `thumbv7em-none-eabi`)
to catch any accidental pointer-size assumptions.

---

## Quick Reference Checklist

Use this to track the implementation status of each architectural component:

### Query overrides
- [ ] `layout_of` — non-generic types
- [ ] `layout_of` — generic types (calls tcx for type arg layouts)
- [ ] `mir_built` — non-generic functions
- [ ] `mir_built` — generic functions (instantiates generic bodies)
- [ ] `mir_borrowck` — selective skip for your language's items
- [ ] `instance_mir` — drop glue for your language's types
- [ ] `needs_drop_raw` — drop need detection for your types

### Type system integration
- [ ] Type oracle binary (replaces rustdoc approach)
- [ ] Inherent method resolution via TyCtxt
- [ ] Trait method resolution via TyCtxt
- [ ] Layout computation uses `tcx.data_layout()` throughout
- [ ] Target-portability tested (32-bit + 64-bit)

### MIR construction
- [ ] Basic function bodies (assignments, returns)
- [ ] Function calls (Rust functions from your language)
- [ ] Drop bodies (trivial + with destructor + generic)
- [ ] StorageLive/StorageDead pairs throughout
- [ ] SourceInfo spans from your language's source files
- [ ] var_debug_info (debugger support)
- [ ] Unwind handling (cleanup blocks, landing pads)

### Build system
- [ ] RUSTC_WRAPPER integration
- [ ] Registry serialization (frontend → driver)
- [ ] Cargo rerun-if-changed for .yourlang files
- [ ] Incremental compilation support
- [ ] Nightly version stamping in registry format
- [ ] CI pipeline with monthly nightly update test

### Safety and correctness
- [ ] Drop chain verified (no double-free, no leak)
- [ ] Boundary invariant documented and tested
- [ ] MIR validator passes with `-Zvalidate-mir`
- [ ] Cycle detection tested (truly cyclic layout gives clear error)
- [ ] Mutually recursive layout tested (3+ levels deep)
