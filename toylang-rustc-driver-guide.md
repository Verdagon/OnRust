# Toylang: A Proof-of-Concept rustc Driver

## What We're Building

A minimal compiler driver that embeds a toy language ("Toylang") inside rustc's compilation
pipeline. Toylang has no generics of its own, but its types can be used as type parameters to
Rust generics — e.g. `Vec<ToylangStruct>` — with correct layout, monomorphization, and drop
behavior, all handled by rustc as if it had known about the type from the start.

By the end of this guide, the following will compile and run correctly:

```rust
// host.rs  (a normal Rust file that uses our toy type)
extern "C" {
    fn make_vec() -> Vec<Point>;
    fn vec_len(v: *const Vec<Point>) -> usize;
}

fn main() {
    let v = unsafe { make_vec() };
    println!("len = {}", unsafe { vec_len(&v) });
}
```

Where `Point` and both functions are defined entirely in Toylang, never written in Rust, and
compiled by our custom driver.

---

## Background: The Five Mechanisms

Architecture C rests on five rustc hooks. This guide implements all five, in order of increasing
complexity. Understanding what each one does before touching code will save a lot of debugging
time.

### Mechanism 1 — `layout_of` override

rustc needs to know the size and alignment of every type it touches, including types that appear
as generic arguments. When rustc computes the layout of `Vec<Point>`, it calls
`tcx.layout_of(Point)`. If `Point` is a Toylang type, that query goes to our custom provider
instead of rustc's default.

Our provider returns a `LayoutS` describing `Point`'s fields, size, and alignment — the same
data structure rustc uses for its own types.

Without this, rustc cannot even compute `size_of::<Vec<Point>>()`, let alone compile code
that touches a `Vec<Point>`.

### Mechanism 2 — `mir_built` override

MIR (Mid-level Intermediate Representation) is the control-flow graph that rustc uses for
analysis and codegen. Every function body — including functions defined in Toylang — must
eventually be represented as a MIR `Body`.

Our `mir_built` override constructs a `Body` from scratch for each Toylang function. rustc
then runs its normal optimization and codegen passes on that body, producing machine code.

Borrow checking is part of the MIR pipeline and would reject our hand-built bodies, so we
disable it for Toylang items (Mechanism 3).

### Mechanism 3 — selective `mir_borrowck` skip

rustc's borrow checker (`mir_borrowck`) runs per-function, keyed by `LocalDefId`. Our override
checks whether the function's source file has a `.toylang` extension. If so, it returns an
empty `BorrowCheckResult` (the "nothing to report" sentinel), skipping all borrow checking for
that item. Normal `.rs` files still go through full borrow checking.

### Mechanism 4 — `drop_in_place` MIR body

When rustc generates drop glue for `Vec<Point>`, it emits a call to
`drop_in_place::<Point>()`. If we don't provide a body for this, rustc will generate a no-op
by default — which is fine for types that don't need destruction, but we need to handle the
general case. Our `mir_built` override also intercepts the synthetic `drop_in_place` shim for
Toylang types and injects a MIR body that calls back into Toylang's destructor function (a
plain `extern "C"` function).

### Mechanism 5 — type oracle query

Before generating any MIR, we need to be able to look up Rust generic APIs against Toylang
types: "what is the signature of `Vec<Point>::push`?" We do this with a small helper that
queries `TyCtxt` directly — `tcx.fn_sig(def_id).instantiate(tcx, args)` — rather than parsing
rustdoc JSON or running a temporary Rust program.

---

## Project Structure

```
toylang/
├── Cargo.toml
├── rust-toolchain.toml          # pins a specific nightly
├── src/
│   ├── main.rs                  # rustc_driver entry point
│   ├── callbacks.rs             # Callbacks trait impl
│   ├── queries/
│   │   ├── mod.rs
│   │   ├── layout.rs            # Mechanism 1
│   │   ├── mir_build.rs         # Mechanisms 2 & 4
│   │   └── borrowck.rs          # Mechanism 3
│   ├── oracle.rs                # Mechanism 5
│   ├── toylang/
│   │   ├── mod.rs
│   │   ├── ast.rs               # Toylang AST types
│   │   ├── parser.rs            # Minimal parser
│   │   └── typeck.rs            # Minimal type checker
│   └── mir_helpers.rs           # Utilities for constructing MIR
├── tests/
│   ├── point.toylang            # Test input
│   └── host.rs                  # Rust code that uses Toylang types
└── README.md
```

---

## Step 0: Toolchain Setup

### `rust-toolchain.toml`

Pin a specific nightly. The `rustc_private` APIs change with every nightly release — pinning
is not optional.

```toml
[toolchain]
channel = "nightly-2025-01-15"   # update this monthly; see section on churn below
components = ["rustc-dev", "rust-src", "llvm-tools-preview"]
```

Check that the components installed correctly:

```bash
rustup component list --installed | grep rustc-dev
# should print: rustc-dev-x86_64-unknown-linux-gnu (or your platform)
```

### `Cargo.toml`

```toml
[package]
name = "toylang"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "toylangc"
path = "src/main.rs"

[dependencies]
# No external dependencies needed for the core driver.
# serde_json is useful for the oracle output if you want readable output.
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[package.metadata.rust-analyzer]
rustc_private = true   # tells rust-analyzer where to find rustc crates
```

---

## Step 1: The Driver Entry Point

### `src/main.rs`

```rust
#![feature(rustc_private)]  // required to use internal rustc crates

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_session;

mod callbacks;
mod queries;
mod oracle;
mod toylang;
mod mir_helpers;

use rustc_driver::RunCompiler;

fn main() {
    // Install the default ICE (Internal Compiler Error) hook so panics in
    // our code produce the same formatted output as rustc's own panics.
    rustc_driver::install_ice_hook(
        "https://github.com/your-org/toylang/issues",
        |_| {},
    );

    let exit_code = rustc_driver::catch_with_exit_code(|| {
        let args: Vec<String> = std::env::args().collect();
        RunCompiler::new(&args, &mut callbacks::ToyCallbacks::new()).run()
    });

    std::process::exit(exit_code);
}
```

At this point the driver is a pass-through — it compiles `.rs` files exactly as `rustc` would.
Verify this works before adding any hooks:

```bash
cargo build
./target/debug/toylangc --edition 2021 tests/host.rs
```

It should produce the same output as running `rustc` directly.

---

## Step 2: The Callbacks Struct

### `src/callbacks.rs`

The `Callbacks` trait has four methods. We only need two: `config` (for registering query
overrides and the file loader) and `after_analysis` (for the type oracle, if we want to
inspect types after the compiler has finished).

```rust
#![allow(unused)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;

use rustc_driver::Compilation;
use rustc_interface::{Config, interface::Compiler};
use rustc_middle::ty::TyCtxt;
use std::sync::Arc;

use crate::toylang::registry::ToylangRegistry;

pub struct ToyCallbacks {
    /// Parsed Toylang definitions, loaded before the rustc session starts.
    /// Arc because the query providers need to access it from within
    /// closures that are passed into the rustc session.
    registry: Arc<ToylangRegistry>,
}

impl ToyCallbacks {
    pub fn new() -> Self {
        // In a real compiler this would parse .toylang files specified on the
        // command line. For now, we hardcode a registry with a single struct.
        Self {
            registry: Arc::new(ToylangRegistry::hardcoded_point()),
        }
    }
}

impl rustc_driver::Callbacks for ToyCallbacks {
    fn config(&mut self, config: &mut Config) {
        // Install the custom file loader so .toylang files can be fed to
        // rustc as if they were Rust source (we return stub Rust source).
        // We'll implement this in Step 5.
        // config.file_loader = Some(Box::new(ToyFileLoader::new(self.registry.clone())));

        let registry = self.registry.clone();

        config.override_queries = Some(move |_session, providers| {
            // Save the original providers so we can fall through to them for
            // non-Toylang items.
            use crate::queries::{borrowck, layout, mir_build};

            providers.layout_of     = layout::layout_of(registry.clone());
            providers.mir_built     = mir_build::mir_built(registry.clone());
            providers.mir_borrowck  = borrowck::mir_borrowck;
        });
    }

    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        // This runs after full analysis (type checking, borrow checking).
        // Good place to run the type oracle or dump debug info.
        if std::env::var("TOYLANG_DUMP_TYPES").is_ok() {
            crate::oracle::dump_all_toylang_types(tcx, &self.registry);
        }
        Compilation::Continue
    }
}
```

Note the use of `Arc<ToylangRegistry>` — the `override_queries` closure is `'static`, so it
cannot borrow from `self`. Wrapping the registry in `Arc` lets us clone a reference into the
closure.

---

## Step 3: The Toylang Registry

Before implementing any query overrides, we need the data structure that describes Toylang
types to rustc.

### `src/toylang/registry.rs`

```rust
use std::collections::HashMap;

/// A Toylang struct field.
#[derive(Clone, Debug)]
pub struct ToyField {
    pub name: String,
    /// The Rust type of this field, as a string that rustc can resolve.
    /// For now we only support primitive Rust types.
    pub rust_type: ToyFieldType,
}

#[derive(Clone, Debug)]
pub enum ToyFieldType {
    I32,
    I64,
    F64,
    Bool,
    // Future: ToyStruct(String) for nested Toylang types
}

impl ToyFieldType {
    pub fn size(&self) -> u64 {
        match self {
            Self::I32  => 4,
            Self::I64  => 8,
            Self::F64  => 8,
            Self::Bool => 1,
        }
    }

    pub fn align(&self) -> u64 {
        match self {
            Self::I32  => 4,
            Self::I64  => 8,
            Self::F64  => 8,
            Self::Bool => 1,
        }
    }
}

/// A Toylang struct definition.
#[derive(Clone, Debug)]
pub struct ToyStruct {
    pub name: String,
    pub fields: Vec<ToyField>,
}

impl ToyStruct {
    /// Total size (with field padding applied).
    pub fn size(&self) -> u64 {
        let mut offset = 0u64;
        for field in &self.fields {
            let align = field.rust_type.align();
            // Pad to field alignment
            offset = (offset + align - 1) & !(align - 1);
            offset += field.rust_type.size();
        }
        // Pad to struct alignment
        let align = self.align();
        (offset + align - 1) & !(align - 1)
    }

    /// Alignment is the max alignment of any field.
    pub fn align(&self) -> u64 {
        self.fields.iter()
            .map(|f| f.rust_type.align())
            .max()
            .unwrap_or(1)
    }

    /// Byte offset of each field after padding.
    pub fn field_offsets(&self) -> Vec<u64> {
        let mut offsets = Vec::new();
        let mut offset = 0u64;
        for field in &self.fields {
            let align = field.rust_type.align();
            offset = (offset + align - 1) & !(align - 1);
            offsets.push(offset);
            offset += field.rust_type.size();
        }
        offsets
    }
}

/// All Toylang definitions visible to the current compilation.
pub struct ToylangRegistry {
    pub structs: HashMap<String, ToyStruct>,
    pub functions: HashMap<String, ToyFunction>,
}

impl ToylangRegistry {
    /// Hardcoded registry for the proof of concept.
    /// Replace this with a real parser in Step 5.
    pub fn hardcoded_point() -> Self {
        let mut structs = HashMap::new();
        structs.insert("Point".to_string(), ToyStruct {
            name: "Point".to_string(),
            fields: vec![
                ToyField { name: "x".to_string(), rust_type: ToyFieldType::I32 },
                ToyField { name: "y".to_string(), rust_type: ToyFieldType::I32 },
            ],
        });

        let mut functions = HashMap::new();
        functions.insert("make_vec".to_string(), ToyFunction {
            name: "make_vec".to_string(),
            // defined in steps 4 and 5
        });

        Self { structs, functions }
    }

    pub fn is_toylang_type(&self, name: &str) -> bool {
        self.structs.contains_key(name)
    }
}

#[derive(Clone, Debug)]
pub struct ToyFunction {
    pub name: String,
    // Will grow to include parameter types, return type, body AST, etc.
}
```

---

## Step 4: Mechanism 1 — `layout_of` Override

This is the most foundational mechanism. Without it, nothing else works.

### How `layout_of` works internally

rustc's layout system lives in `rustc_middle::ty::layout`. The key type is
`LayoutS<FieldIdx>`, which stores:

- `size: Size` — total byte size
- `align: AbiAndPrefAlign` — ABI and preferred alignment
- `fields: FieldsShape` — how fields are arranged (offsets, indices)
- `abi: Abi` — how the type is passed across function call boundaries
- `variants: Variants` — for enums; `Variants::Single` for structs

For a simple struct with primitive fields, we need to fill all of these in correctly.
Incorrect layouts cause silent memory corruption, not a compile error — rustc trusts the
provider to be accurate.

### `src/queries/layout.rs`

```rust
extern crate rustc_abi;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_target;

use std::sync::Arc;
use rustc_middle::ty::{TyCtxt, ParamEnvAnd, Ty};
use rustc_middle::ty::layout::TyAndLayout;
use rustc_abi::{
    Abi, AbiAndPrefAlign, Align, FieldsShape, LayoutS, Primitive, Scalar,
    Size, Variants, WrappingRange,
};
use rustc_middle::query::Providers;
use crate::toylang::registry::ToylangRegistry;

/// Returns a custom `layout_of` provider that handles Toylang types.
/// For non-Toylang types, falls through to rustc's default provider.
pub fn layout_of(
    registry: Arc<ToylangRegistry>,
) -> for<'tcx> fn(TyCtxt<'tcx>, ParamEnvAnd<'tcx, Ty<'tcx>>) -> Result<TyAndLayout<'tcx>, ...> {
    // Note: Rust doesn't support closures as function pointers when they capture
    // state. We use thread-local storage to work around this.
    //
    // A cleaner approach (used by Kani) is to store the registry in a thread_local
    // that is populated before the compiler session starts.
    todo!("see note below about thread_local pattern")
}
```

**Important implementation note:** `override_queries` requires a plain function pointer
(`fn(TyCtxt<'tcx>, ...) -> ...`), not a closure. This means you cannot capture `registry`
directly. The standard pattern used by Kani, Prusti, and other drivers is to store
shared state in a `thread_local!`:

```rust
// At the top of queries/layout.rs:
use std::cell::RefCell;
use std::sync::Arc;
use crate::toylang::registry::ToylangRegistry;

thread_local! {
    static REGISTRY: RefCell<Option<Arc<ToylangRegistry>>> = RefCell::new(None);
}

pub fn install_registry(registry: Arc<ToylangRegistry>) {
    REGISTRY.with(|r| *r.borrow_mut() = Some(registry));
}

fn with_registry<R>(f: impl FnOnce(&ToylangRegistry) -> R) -> R {
    REGISTRY.with(|r| {
        f(r.borrow().as_ref().expect("registry not installed"))
    })
}
```

Call `install_registry` from `ToyCallbacks::config` before `override_queries` runs.

Then the actual provider:

```rust
pub fn toy_layout_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    query: rustc_middle::ty::ParamEnvAnd<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, rustc_middle::ty::layout::LayoutError<'tcx>> {

    let ty = query.value;

    // Check if this is a Toylang type by inspecting the type's name.
    // A cleaner approach (Steps 5+) is to track DefIds directly.
    let type_name = with_registry(|reg| {
        // We need to match the Ty to one of our registered structs.
        // For now, use the debug representation as a heuristic.
        let name = format!("{:?}", ty);
        reg.structs.keys()
            .find(|k| name.contains(k.as_str()))
            .cloned()
    });

    if let Some(struct_name) = type_name {
        return with_registry(|reg| {
            let toy_struct = &reg.structs[&struct_name];
            Ok(build_layout_for_toy_struct(tcx, ty, toy_struct))
        });
    }

    // Not a Toylang type — call rustc's default provider.
    // We saved it before installing ours.
    (DEFAULT_LAYOUT_OF)(tcx, query)
}

fn build_layout_for_toy_struct<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
    toy: &crate::toylang::registry::ToyStruct,
) -> TyAndLayout<'tcx> {
    let size = Size::from_bytes(toy.size());
    let align = Align::from_bytes(toy.align()).unwrap();
    let abi_align = AbiAndPrefAlign::new(align);

    let offsets: Vec<Size> = toy.field_offsets()
        .iter()
        .map(|&o| Size::from_bytes(o))
        .collect();

    let layout = LayoutS {
        fields: FieldsShape::Arbitrary {
            offsets: offsets.into(),
            memory_index: (0..toy.fields.len() as u32).collect(),
        },
        variants: Variants::Single { index: rustc_abi::VariantIdx::from_u32(0) },
        abi: Abi::Aggregate { sized: true },
        largest_niche: None,
        align: abi_align,
        size,
        max_repr_align: None,
        unadjusted_abi_align: align,
    };

    TyAndLayout {
        ty,
        layout: tcx.intern_layout(layout),
    }
}
```

**Verify this works** by adding a test that prints `size_of::<Point>()` from Rust. It should
match what the Toylang registry computes.

---

## Step 5: Mechanism 3 — Selective `mir_borrowck` Skip

Implement this before `mir_built` because without it, any MIR body we inject will be rejected
by the borrow checker with confusing errors.

### `src/queries/borrowck.rs`

```rust
extern crate rustc_borrowck;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_middle::ty::TyCtxt;
use rustc_middle::mir::BorrowCheckResult;
use rustc_hir::def_id::LocalDefId;

pub fn mir_borrowck<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> &'tcx BorrowCheckResult<'tcx> {
    if is_toylang_item(tcx, def_id) {
        // Return an empty result — no borrow errors, no region info.
        // This is equivalent to what rustc returns for items in
        // #[rustc_macro_transparency] or with #[custom_mir].
        tcx.arena.alloc(BorrowCheckResult {
            concrete_opaque_types: Default::default(),
            closure_requirements: None,
            used_mut_upvars: Default::default(),
            tainted_by_errors: None,
        })
    } else {
        // Normal borrow checking for Rust items.
        (DEFAULT_MIR_BORROWCK)(tcx, def_id)
    }
}

fn is_toylang_item(tcx: TyCtxt<'_>, def_id: LocalDefId) -> bool {
    let span = tcx.def_span(def_id);
    let source_map = tcx.sess.source_map();
    let file = source_map.lookup_source_file(span.lo());
    file.name
        .prefer_local()
        .to_string_lossy()
        .ends_with(".toylang")
}
```

This is the cleanest of the five mechanisms. The only subtlety is `DEFAULT_MIR_BORROWCK` —
save the default provider before overriding it, in the same way we saved `DEFAULT_LAYOUT_OF`.

---

## Step 6: Mechanism 2 — `mir_built` Override

This is the most complex mechanism. We construct a `Body<'tcx>` — rustc's MIR data structure
— from scratch.

### Understanding the MIR `Body` structure

A `Body` is a control-flow graph. The key fields:

```
Body {
    basic_blocks: IndexVec<BasicBlock, BasicBlockData>
    local_decls:  IndexVec<Local, LocalDecl>    // variables (args + temps + return)
    arg_count:    usize
    ...
}
```

**Locals:** `Local(0)` is always the return place (`_0`). `Local(1)` through
`Local(arg_count)` are function arguments. Remaining locals are temporaries.

**Basic blocks:** Each `BasicBlockData` has:
- `statements: Vec<Statement>` — assignments, storage markers, etc.
- `terminator: Terminator` — how control leaves the block (`Return`, `Goto`, `Call`, etc.)

**The simplest valid MIR body** (for a function that returns a constant):

```
bb0:
  _0 = const 42_i32;
  return;
```

In Rust API terms:

```rust
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;

fn build_trivial_body<'tcx>(tcx: TyCtxt<'tcx>, span: Span) -> Body<'tcx> {
    // _0: i32  (return value)
    let return_local = LocalDecl::new(tcx.types.i32, span);

    let assign_stmt = Statement {
        source_info: SourceInfo::outermost(span),
        kind: StatementKind::Assign(Box::new((
            Place::return_place(),
            Rvalue::Use(Operand::Constant(Box::new(ConstOperand {
                span,
                user_ty: None,
                const_: Const::Val(
                    ConstValue::Scalar(Scalar::from_i32(42)),
                    tcx.types.i32,
                ),
            }))),
        ))),
    };

    let terminator = Terminator {
        source_info: SourceInfo::outermost(span),
        kind: TerminatorKind::Return,
    };

    let bb = BasicBlockData {
        statements: vec![assign_stmt],
        terminator: Some(terminator),
        is_cleanup: false,
    };

    let mut basic_blocks = IndexVec::new();
    basic_blocks.push(bb);

    let mut local_decls = IndexVec::new();
    local_decls.push(return_local);  // Local(0) = return place

    Body::new(
        MirSource::item(def_id.to_def_id()),
        basic_blocks,
        IndexVec::from_elem_n(
            SourceScopeData {
                span,
                parent_scope: None,
                inlined: None,
                inlined_parent_scope: None,
                local_data: ClearCrossCrate::Clear,
            },
            1,
        ),
        local_decls,
        IndexVec::new(),  // user_type_annotations
        0,                // arg_count (no arguments)
        vec![],           // var_debug_info
        span,
        None,             // coroutine
        None,             // tainted_by_errors
    )
}
```

### Gotchas the MIR validator will catch

rustc runs `validate_mir` even with borrowck disabled. Common failures:

1. **Missing `StorageLive`/`StorageDead` pairs.** Every local (except `_0` and args) must
   have a `StorageLive` statement before first use and a `StorageDead` after last use.
   Skip this and you'll see: `StorageLive(_N) not found`.

2. **Wrong `SourceInfo` spans.** Every statement and terminator needs a non-dummy `SourceInfo`.
   Use `SourceInfo::outermost(span)` where `span` is the definition span of the function.
   Using `DUMMY_SP` sometimes works but triggers span-related ICEs later.

3. **Return type mismatch.** `Local(0)`'s type must exactly match the function's declared
   return type. Check `tcx.fn_sig(def_id).output()`.

4. **Terminator missing from last block.** Every `BasicBlockData` must have
   `terminator: Some(...)`. A `None` terminator panics during codegen.

5. **`Call` terminator's `destination` type.** The place you're writing the call result into
   must have the same type as the callee's return type. Use `tcx.fn_sig` to get the return
   type, then create a local with that type.

### Building a `Vec<Point>::push` call in MIR

This is what a `Vec<Point>::push(v, point)` call looks like in MIR:

```
bb0:
  StorageLive(_3);
  _3 = move _2;                    // move the Point into a temp
  _0 = Vec::<Point>::push(_1, move _3) -> bb1;

bb1:
  StorageDead(_3);
  return;
```

The `Call` terminator:

```rust
TerminatorKind::Call {
    func: Operand::function_handle(
        tcx,
        push_def_id,       // DefId of Vec::push
        tcx.mk_args(&[     // GenericArgs: T = Point
            GenericArg::from(point_ty),
        ]),
        span,
    ),
    args: vec![
        Spanned { node: Operand::Move(Place::from(vec_local)), span },
        Spanned { node: Operand::Move(Place::from(point_local)), span },
    ],
    destination: Place::return_place(),
    target: Some(BasicBlock::from_u32(1)),   // bb1
    unwind: UnwindAction::Continue,
    call_source: CallSource::Normal,
    fn_span: span,
}
```

To find `push_def_id`, use the type oracle (Mechanism 5).

### `src/queries/mir_build.rs` — skeleton

```rust
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_hir;

use rustc_middle::ty::TyCtxt;
use rustc_middle::mir::Body;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::util::steal::Steal;

pub fn toy_mir_built<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> &'tcx Steal<Body<'tcx>> {

    if !is_toylang_item(tcx, def_id) {
        return (DEFAULT_MIR_BUILT)(tcx, def_id);
    }

    let span = tcx.def_span(def_id);
    let fn_name = tcx.item_name(def_id.to_def_id()).to_string();

    let body = match fn_name.as_str() {
        "make_vec" => build_make_vec(tcx, def_id, span),
        "vec_len"  => build_vec_len(tcx, def_id, span),
        // drop_in_place for Point: handled separately below
        name => panic!("toylang: unknown function '{}'", name),
    };

    tcx.arena.alloc(Steal::new(body))
}
```

For `drop_in_place::<Point>()`, the `DefId` is a synthetic shim generated by rustc, not a
user-defined function. Intercept it by checking
`tcx.def_kind(def_id) == DefKind::SyntheticCoroutineBody` — actually check for
`InstanceKind::DropGlue` when the query comes through the `instance_mir` path. This is
described in detail in the drop glue section below.

---

## Step 7: Mechanism 4 — Drop Glue for Toylang Types

### How rustc generates drop glue

When a type `T` is dropped, rustc calls `drop_in_place::<T>()`. For types with no `Drop`
impl and no fields that need dropping, this is a no-op. For types with fields that need
dropping, rustc generates a MIR body that drops each field in turn.

For Toylang types, we want to call a Toylang-provided destructor function — a plain
`extern "C" fn` that Toylang's runtime will supply.

### The intercept point

Drop glue is requested via `tcx.instance_mir(InstanceKind::DropGlue(def_id, Some(ty)))`.
Override this query to intercept drop glue for Toylang types:

```rust
providers.instance_mir = |tcx, instance| {
    if let InstanceKind::DropGlue(_, Some(ty)) = instance {
        if is_toylang_ty(tcx, ty) {
            return build_toylang_drop_glue(tcx, ty);
        }
    }
    DEFAULT_INSTANCE_MIR(tcx, instance)
};
```

### Building the drop MIR body

For the proof of concept, a Toylang type's drop body calls an `extern "C"` function with
signature `fn(*mut Point)`:

```
// MIR for drop_in_place::<Point>()
// arg: _1: *mut Point

bb0:
  __toylang_drop_Point(_1 as *mut ()) -> bb1;

bb1:
  return;
```

Where `__toylang_drop_Point` is declared as:

```rust
extern "C" {
    fn __toylang_drop_Point(ptr: *mut ());
}
```

And implemented in a small C or Toylang runtime file that gets linked in. For the proof of
concept, just print a message and do nothing:

```c
// runtime.c
#include <stdio.h>
void __toylang_drop_Point(void* ptr) {
    printf("dropping Point at %p\n", ptr);
}
```

This verifies the drop chain fires correctly without needing a real destructor.

### Why this is important even for trivial types

Even if `Point` has no destructor, registering the drop glue mechanism now means:

- `Vec<Point>` will compile without linker errors about missing `drop_in_place`
- You have a working template for types that *do* need destruction
- The drop call chain (Rust drops `Vec<Point>` → calls `drop_in_place::<Point>()` → calls
  Toylang's destructor) is verified end-to-end

---

## Step 8: Mechanism 5 — Type Oracle

The type oracle lets us answer: "what is the signature of `Vec<Point>::push`?" using
`TyCtxt` directly.

### `src/oracle.rs`

```rust
extern crate rustc_middle;
extern crate rustc_span;

use rustc_middle::ty::{TyCtxt, GenericArg, Ty};
use rustc_span::symbol::Symbol;

/// Find the DefId of Vec::push and return its fully instantiated signature
/// with T = Point.
pub fn resolve_vec_push<'tcx>(
    tcx: TyCtxt<'tcx>,
    element_ty: Ty<'tcx>,
) -> Option<rustc_middle::ty::FnSig<'tcx>> {
    // Step 1: Find the DefId of std::vec::Vec
    let vec_def_id = find_type_def_id(tcx, &["std", "vec", "Vec"])?;

    // Step 2: Find the inherent impl of Vec that contains `push`
    let push_def_id = tcx
        .inherent_impls(vec_def_id)
        .iter()
        .flat_map(|&impl_id| tcx.associated_item_def_ids(impl_id))
        .find(|&&item_id| {
            tcx.item_name(item_id) == Symbol::intern("push")
        })?;

    // Step 3: Instantiate the signature with T = element_ty
    let generic_args = tcx.mk_args(&[GenericArg::from(element_ty)]);
    let sig = tcx
        .fn_sig(*push_def_id)
        .instantiate(tcx, generic_args);

    Some(tcx.normalize_erasing_regions(
        rustc_middle::ty::ParamEnv::reveal_all(),
        sig.skip_binder(),
    ))
}

fn find_type_def_id(
    tcx: TyCtxt<'_>,
    path: &[&str],
) -> Option<rustc_hir::def_id::DefId> {
    // Walk the crate graph looking for the type.
    // In practice, use tcx.def_path_hash_to_def_id or
    // tcx.get_diagnostic_item for well-known types.
    let vec_symbol = rustc_span::sym::Vec;
    tcx.get_diagnostic_item(vec_symbol)
}

/// Dump all Toylang types and their resolved sizes for debugging.
pub fn dump_all_toylang_types(
    tcx: TyCtxt<'_>,
    registry: &crate::toylang::registry::ToylangRegistry,
) {
    for (name, toy_struct) in &registry.structs {
        println!("[oracle] {} : size={} align={}",
            name, toy_struct.size(), toy_struct.align());

        // Try to resolve Vec<ToyStruct>::push as a sanity check
        // (requires actually constructing the Ty, not just having the name)
        println!("[oracle]   fields: {:?}",
            toy_struct.fields.iter()
                .map(|f| format!("{}: {:?}", f.name, f.rust_type))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}
```

The key call is `tcx.fn_sig(def_id).instantiate(tcx, generic_args)`. This runs rustc's
type substitution engine — the same one it uses during monomorphization — so overload
resolution, trait bound checking, and type inference are all handled correctly. You don't
need to reimplement any of it.

---

## Step 9: End-to-End Test

With all five mechanisms in place, write a complete test that exercises the full pipeline.

### `tests/point.toylang`

```
// Toylang source for the Point struct and associated functions.
// For now this file exists as documentation; the registry is hardcoded.
// Step 10 wires up a real parser.

struct Point {
    x: i32,
    y: i32,
}

fn make_vec() -> Vec<Point> {
    let v = Vec<Point>::new();
    v.push(Point { x: 1, y: 2 });
    v.push(Point { x: 3, y: 4 });
    v
}

fn vec_len(v: &Vec<Point>) -> i64 {
    v.len() as i64
}
```

### `tests/host.rs`

```rust
extern "C" {
    fn make_vec() -> Vec<Point>;
    fn vec_len(v: *const Vec<Point>) -> i64;
}

// Point must be declared here too, as an opaque zero-sized marker.
// Its actual layout comes from our driver's layout_of override.
#[repr(transparent)]
struct Point([u8; 8]);  // size must match Toylang's Point { x: i32, y: i32 }

fn main() {
    unsafe {
        let v = make_vec();
        let len = vec_len(&v);
        println!("Vec length: {}", len);
        assert_eq!(len, 2);
    }
}
```

### Build script

```bash
# Compile the Toylang runtime (drop glue callbacks)
gcc -c tests/runtime.c -o tests/runtime.o

# Compile the test using our custom driver
./target/debug/toylangc \
    --edition 2021 \
    --crate-type bin \
    tests/host.rs \
    tests/runtime.o \
    -o tests/out

# Run
./tests/out
# Expected:
# dropping Point at 0x...   (printed twice, once per element)
# Vec length: 2
```

---

## Step 10: Wiring Up a Real Parser (Minimal)

Rather than hardcoding the registry, parse a real `.toylang` file.

The minimal parser only needs to handle struct definitions and function signatures. Function
bodies can stay as a stub for now — full MIR construction from an AST is the next milestone.

### Grammar (EBNF)

```
program     = item*
item        = struct_def | fn_def
struct_def  = "struct" IDENT "{" field_list "}"
field_list  = (field ("," field)*)? ","?
field       = IDENT ":" type
fn_def      = "fn" IDENT "(" param_list? ")" ("->" type)? "{" "}"
param_list  = param ("," param)*
param       = IDENT ":" type
type        = IDENT ("<" type_args ">")?
type_args   = type ("," type)*
```

No expressions, no statements, no control flow — just enough to populate the registry.

### Parser output

The parser produces a `ToylangRegistry`. The registry is then passed to the driver as before.

```bash
# Invocation with a real .toylang file
./target/debug/toylangc \
    --edition 2021 \
    --toylang-input tests/point.toylang \   # new flag, parsed by our main()
    tests/host.rs \
    -o tests/out
```

Parse `--toylang-input` in `main.rs` before calling `RunCompiler::new`, populate the
registry from the parsed AST, and continue as before.

---

## Handling Nightly API Churn

The `rustc_private` API changes with every nightly release. Here is how to manage it.

### Pin strictly

The `rust-toolchain.toml` pins the exact nightly. Do not use `channel = "nightly"` without
a date. CI should fail if the toolchain file is not respected.

### Update monthly

Pick a day each month to update the pin. The update process:

1. Change the date in `rust-toolchain.toml`
2. Run `cargo build`
3. Fix any compilation errors (usually renamed types or changed method signatures)
4. Run the full test suite
5. Commit the toolchain bump as a standalone commit for easy bisection

### What typically changes between nightlies

- `LayoutS` field names and constructor signatures
- `TerminatorKind` variant fields (especially around async/generators)
- `Providers` struct gaining new fields (safe to ignore — just add the override you need)
- `BorrowCheckResult` fields

What almost never changes:

- The `Callbacks` trait interface
- `override_queries` mechanism
- `TyCtxt` query names (`layout_of`, `mir_built`, `mir_borrowck`)
- `Body`, `BasicBlock`, `Statement`, `Terminator` structure

### Track the Stable MIR project

The `rustc_public` initiative (https://github.com/rust-lang/project-stable-mir) aims to
stabilize the subset of `TyCtxt` that external tools need. Once stable, you can drop
`#![feature(rustc_private)]` and the nightly requirement entirely. Target: 2025–2026.
Monitor the project's GitHub for stabilization announcements.

---

## Debugging Reference

### Useful environment variables

```bash
# Dump MIR for all functions after our overrides are applied
RUSTFLAGS="-Zdump-mir=all" ./target/debug/toylangc ...

# Enable MIR validation (catches structural errors in our injected bodies)
RUSTFLAGS="-Zvalidate-mir" ./target/debug/toylangc ...

# Pretty-print the HIR (useful for understanding how rustc sees the Rust side)
RUSTFLAGS="-Zunpretty=hir-tree" ./target/debug/toylangc ...

# Show all query executions (very verbose, but useful for tracing layout_of calls)
RUSTC_LOG=rustc_query_system=debug ./target/debug/toylangc ... 2>&1 | grep layout_of
```

### Diagnosing "ICE: unexpected type" errors

These usually mean our `layout_of` provider returned a layout inconsistent with the type's
Rust declaration. Double-check:
- `FieldsShape::Arbitrary` offsets match actual struct layout (including padding)
- `Abi` variant is correct (`Abi::Aggregate` for structs)
- `size` and `align` in the `LayoutS` are consistent with each other

### Diagnosing "MIR validation error"

Run with `-Zvalidate-mir` to get the specific error. Common ones:

| Error | Cause |
|-------|-------|
| `StorageLive(_N) not found` | Missing `StorageLive` statement before first use of local N |
| `return type mismatch` | `Local(0)`'s type doesn't match `tcx.fn_sig(def_id).output()` |
| `use of uninitialized local` | A local is read before being assigned |
| `terminator missing` | A `BasicBlockData` has `terminator: None` |

### Confirming layout_of is being called

Add a `println!` inside `toy_layout_of` and check that it fires when compiling the test:

```rust
pub fn toy_layout_of<'tcx>(tcx: TyCtxt<'tcx>, query: ...) -> ... {
    let ty = query.value;
    eprintln!("[toylang] layout_of called for: {:?}", ty);
    // ...
}
```

You should see this line when rustc computes the layout of `Vec<Point>`.

---

## Summary Checklist

Use this to track progress:

- [ ] **Step 0** — Toolchain pinned, `cargo build` succeeds, pass-through driver works
- [ ] **Step 1** — `ToyCallbacks` compiles, `override_queries` registered (no-op overrides)
- [ ] **Step 2** — `ToylangRegistry` with hardcoded `Point` struct
- [ ] **Step 3** — `layout_of` override: `size_of::<Point>()` returns 8 from Rust code
- [ ] **Step 4** — `mir_borrowck` skip: no borrow check errors on injected MIR stubs
- [ ] **Step 5** — `mir_built` override: trivial `get_x()` function compiles and returns correct value
- [ ] **Step 6** — Drop glue: `Vec<Point>` drops without crash; runtime.c destructor fires
- [ ] **Step 7** — Type oracle: `Vec<Point>::push` signature resolved via `TyCtxt`
- [ ] **Step 8** — End-to-end test: `make_vec` / `vec_len` compile and produce correct output
- [ ] **Step 9** — Real parser: `point.toylang` parsed instead of hardcoded registry
- [ ] **Step 10** — All tests pass after a nightly version bump (validates churn resilience)
