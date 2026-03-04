# Toylang: A Comprehensive Guide to the rustc Driver

This is what's already implemented.

## What This Is

`toylangc` is a custom Rust compiler driver — a binary that calls into `rustc` as a library
and intercepts specific parts of the compilation pipeline. It implements a toy language
("Toylang") whose types and functions are injected directly into rustc's internal data
structures, so they compile alongside normal Rust code as if they had been written in Rust
all along.

The end result: Rust source files can declare `fn make_vec() -> Vec<Point>` with a stub body
(`unreachable!()`), point the driver at a `point.toylang` file, and get a working binary
where `make_vec` actually builds a `Vec<Point>` with two elements. The Rust side never sees
the implementation — it's synthesized entirely inside the compiler.

---

## The Big Picture

Normal `rustc` is a binary. This project turns `rustc` into a library (`rustc_driver`) and
wraps it with a thin binary that hooks into five specific query points:

```
toylangc [--toylang-input point.toylang] tests/host.rs -o /tmp/out
    │
    ├─ parse point.toylang → ToylangRegistry
    │
    └─ RunCompiler::new(&args, &mut ToyCallbacks)
           │
           ├─ config() → override_queries:
           │      layout_of    → toy_layout_of
           │      mir_built    → toy_mir_built
           │      mir_borrowck → toy_mir_borrowck
           │      mir_shims    → toy_mir_shims
           │
           └─ normal rustc compilation (parse, HIR, type check, MIR, codegen)
                  │
                  ├─ layout_of(Point)      → intercepted → synthetic LayoutData
                  ├─ mir_built(make_vec)   → intercepted → synthesized from AST
                  ├─ mir_built(vec_len)    → intercepted → synthesized from AST
                  ├─ mir_borrowck(*)       → intercepted → skip for .toylang items
                  └─ mir_shims(DropGlue)  → intercepted → calls __toylang_drop_Point
```

Each hook intercepts a specific rustc *query* — a pure function from input to output that
rustc memoizes in a query cache. By replacing a query's provider function, we control what
data rustc uses for any type or function we claim ownership of.

---

## Project Structure

```
src/
├── main.rs               — entry point: parse .toylang file, launch RunCompiler
├── callbacks.rs          — ToyCallbacks: installs query overrides
├── oracle.rs             — type oracle: resolve Vec method DefIds and signatures
├── mir_helpers.rs        — hand-written MIR body builders (used by drop glue, get_x)
├── queries/
│   ├── mod.rs            — toy_override_queries: saves defaults, installs all overrides
│   ├── layout.rs         — layout_of override
│   ├── mir_build.rs      — mir_built override (dispatch to lower.rs or fallback)
│   ├── borrowck.rs       — mir_borrowck override (skip for Toylang items)
│   └── drop_glue.rs      — mir_shims override (DropGlue for Toylang types)
└── toylang/
    ├── mod.rs
    ├── ast.rs            — Expr, Stmt, FnBody AST nodes
    ├── parser.rs         — lexer + recursive-descent parser for .toylang files
    ├── registry.rs       — ToylangRegistry: ToyStruct, ToyFunction, ToyField
    └── lower.rs          — AST → MIR lowering: build_body()

tests/
├── point.toylang         — Toylang source: struct Point, make_vec, vec_len
├── host.rs               — main test: calls make_vec / vec_len, checks output
├── mir_test.rs           — regression test: get_x returns 42 (hardcoded MIR)
├── drop_test.rs          — regression test: drop glue fires for Point
├── runtime.c             — __toylang_drop_Point: prints drop address
└── runtime.o             — precompiled runtime.c
```

---

## Part 1: The Driver Model

### How `rustc_driver` Works

`rustc` is structured as a library (`rustc_driver`). Any binary can call it, supply a
`Callbacks` implementation, and hook into the compilation process:

```rust
// src/main.rs
rustc_driver::catch_with_exit_code(|| {
    RunCompiler::new(&args, &mut callbacks::ToyCallbacks::new(registry)).run()
})
```

`RunCompiler::run()` executes the full rustc pipeline. The callbacks are called at two points:

- **`config(&mut Config)`** — called before the compiler session starts. This is where we
  install query provider overrides. It also gets a `&mut Config` where we could install a
  custom file loader (not currently used).

- **`after_analysis(tcx)`** — called after type checking and borrow checking complete, before
  codegen. Used here for the optional type oracle dump (`TOYLANG_DUMP_TYPES=1`).

### The `#![feature(rustc_private)]` Gate

All internal rustc crates (`rustc_middle`, `rustc_abi`, etc.) are gated behind
`#![feature(rustc_private)]`. This must appear in `src/main.rs` and each file that uses
internal crates must re-declare them with `extern crate`:

```rust
extern crate rustc_middle;
extern crate rustc_abi;
// etc.
```

This is because internal crates don't appear in `Cargo.toml` — they're resolved by the
nightly toolchain's sysroot. The `[package.metadata.rust-analyzer] rustc_private = true`
setting in `Cargo.toml` tells rust-analyzer where to find them.

### Toolchain Pinning

Internal APIs change with every nightly. The project pins `nightly-2025-01-15` in
`rust-toolchain.toml`. Bumping this requires auditing changed struct fields in `LayoutData`,
`BorrowCheckResult`, etc.

---

## Part 2: Startup Sequence

### `main.rs` — Argument Parsing and Registry

Before handing off to rustc, `main.rs` intercepts the custom `--toylang-input` flag:

```rust
fn extract_registry(args: &mut Vec<String>) -> ToylangRegistry {
    if let Some(pos) = args.iter().position(|a| a == "--toylang-input") {
        if pos + 1 < args.len() {
            let path = args[pos + 1].clone();
            args.drain(pos..=pos + 1);   // remove the flag before passing to rustc
            let src = std::fs::read_to_string(&path)...;
            return crate::toylang::parser::parse(&src)...;
        }
    }
    ToylangRegistry::hardcoded_point()   // fallback for tests without a .toylang file
}
```

The `--toylang-input` and its path argument are stripped from `args` before `RunCompiler`
sees them — rustc would error on unknown flags. The resulting `ToylangRegistry` is wrapped
in `Arc` and threaded through the rest of the system.

### `callbacks.rs` — Installing the Hooks

`ToyCallbacks::config` does two things:

1. **Installs the registry** into each query module's `OnceLock<Arc<ToylangRegistry>>` static.
   This must happen before `override_queries` because the overrides read from those statics.

2. **Sets `config.override_queries`** to `toy_override_queries`, a plain function pointer
   (not a closure — closures cannot be used here because `override_queries` is `fn(...)`,
   not `Box<dyn Fn(...)>`).

```rust
fn config(&mut self, config: &mut Config) {
    layout::install_registry(self.registry.clone());
    borrowck::install_registry(self.registry.clone());
    mir_build::install_registry(self.registry.clone());
    drop_glue::install_registry(self.registry.clone());
    config.override_queries = Some(crate::queries::toy_override_queries);
}
```

### `queries/mod.rs` — The Override Hook

`toy_override_queries` saves each default provider before replacing it:

```rust
pub fn toy_override_queries(_session: &Session, providers: &mut Providers) {
    layout::save_default(providers.layout_of);
    borrowck::save_default(providers.mir_borrowck);
    mir_build::save_default(providers.mir_built);
    drop_glue::save_default(providers.mir_shims);

    providers.layout_of    = layout::toy_layout_of;
    providers.mir_borrowck = borrowck::toy_mir_borrowck;
    providers.mir_built    = mir_build::toy_mir_built;
    providers.mir_shims    = drop_glue::toy_mir_shims;
}
```

Saving the defaults is critical: for any type or function that isn't ours, we must delegate
to rustc's original implementation. Not doing so would break all of `std`.

### The `OnceLock` Pattern for Shared State

Query providers must be plain function pointers (`fn(TyCtxt, ...) -> ...`), not closures.
They cannot capture state. To share the registry with them, we use `OnceLock<Arc<...>>`
module-level statics:

```rust
static REGISTRY: OnceLock<Arc<ToylangRegistry>> = OnceLock::new();
static DEFAULT_LAYOUT_OF: OnceLock<LayoutOfFn> = OnceLock::new();
```

`OnceLock` (rather than `thread_local!`) is used because rustc may invoke queries on Rayon
worker threads. `thread_local!` would only be populated on the main thread, causing
`None` on worker threads. `OnceLock` is set once (during `config`) and thereafter read-only,
so it's safe to access from any thread.

---

## Part 3: The Toylang Language

### The Registry

`ToylangRegistry` is the central data structure that represents a parsed `.toylang` file:

```rust
pub struct ToylangRegistry {
    pub structs:   HashMap<String, ToyStruct>,
    pub functions: HashMap<String, ToyFunction>,
}

pub struct ToyStruct {
    pub name:   String,
    pub fields: Vec<ToyField>,    // ordered list of fields
}

pub struct ToyField {
    pub name:      String,
    pub rust_type: ToyFieldType,  // I32 | I64 | F64 | Bool
}

pub struct ToyFunction {
    pub name:      String,
    pub params:    Vec<ToyParam>,
    pub return_ty: Option<String>,
    pub body:      Option<FnBody>,  // None for hardcoded-fallback functions
}
```

The registry is immutable after construction. All five query overrides read from it but
never write to it.

### The `.toylang` File Format

A `.toylang` file contains struct definitions and function definitions:

```toylang
struct Point {
    x: i32,
    y: i32,
}

fn make_vec() -> Vec<Point> {
    let v = Vec::new();
    v.push(Point { x: 1, y: 2 });
    v.push(Point { x: 3, y: 4 });
    v
}

fn vec_len(v: &Vec<Point>) -> usize {
    v.len()
}
```

Supported field types: `i32`, `i64`, `f64`, `bool`.
Supported expressions: integer literals, variables, static calls (`Vec::new()`), method calls
(`v.push(x)`, `v.len()`), and struct literals (`Point { x: 1, y: 2 }`).

### The Lexer

The lexer in `parser.rs` produces a flat `Vec<Token>`:

```rust
enum Token {
    Ident(String),    // identifiers and keywords
    IntLit(i64),      // 0–9...
    LBrace, RBrace,   // { }
    LParen, RParen,   // ( )
    LAngle, RAngle,   // < >
    Colon,            // :   (single colon — struct field separator)
    DoubleColon,      // ::  (checked before Colon — static call separator)
    Comma,            // ,
    Ampersand,        // &
    Star,             // *
    Arrow,            // ->  (return type)
    Dot,              // .   (method call)
    Semicolon,        // ;
    Equals,           // =
    Eof,
}
```

Two-character tokens (`->`, `::`) are handled before single-character tokens by peeking at
the next character. Integer literals are recognized by a digit-sequence scan. Unknown
characters are silently skipped (for future whitespace robustness).

### The Parser

The parser is a hand-written recursive descent parser. The grammar it handles:

```
program     = (struct_def | fn_def)*
struct_def  = "struct" IDENT "{" (field ("," field)* ","?)? "}"
field       = IDENT ":" primitive_type
fn_def      = "fn" IDENT "(" params ")" ("->" type_str)? "{" fn_body "}"
params      = (IDENT ":" type_str ("," IDENT ":" type_str)*)?
type_str    = "&" "mut"? type_str | "*" ("const"|"mut") type_str
            | IDENT ("<" type_str ("," type_str)* ">")?

fn_body     = stmt* trailing_expr?
stmt        = "let" IDENT "=" expr ";"
            | expr ";"
trailing_expr = expr   // no semicolon — becomes the return value

expr        = primary ("." IDENT "(" args ")")*
primary     = IntLit
            | IDENT "::" IDENT "(" args ")"   // static call
            | IDENT "{" (IDENT ":" expr ("," IDENT ":" expr)*)? "}"  // struct lit
            | IDENT                             // variable
args        = (expr ("," expr)*)?
```

The parser produces a `ToylangRegistry` where each `ToyFunction` has a `body: Some(FnBody)`.
The hardcoded fallback registry (`ToylangRegistry::hardcoded_point()`) sets `body: None`.

### The AST

`src/toylang/ast.rs` defines the AST nodes:

```rust
pub enum Expr {
    IntLit(i64),
    Var(String),
    StaticCall  { ty: String, method: String, args: Vec<Expr> },
    MethodCall  { receiver: Box<Expr>, method: String, args: Vec<Expr> },
    StructLit   { name: String, fields: Vec<(String, Expr)> },
}

pub enum Stmt {
    Let { name: String, expr: Expr },
    ExprStmt(Expr),
}

pub struct FnBody {
    pub stmts: Vec<Stmt>,
    pub ret:   Option<Expr>,   // trailing expression — becomes return value
}
```

---

## Part 4: The Five Query Overrides

### Overview of rustc's Query System

rustc's query system is a memoized, demand-driven computation graph. When the compiler needs
to know, for example, the layout of `Vec<Point>`, it calls
`tcx.layout_of(ParamEnv::empty().and(vec_point_ty))`. This goes through a dispatch table
(`Providers`) that maps each query to a function. Normally all entries point to rustc's
built-in implementations. We replace four of them.

---

### Override 1: `layout_of` — Teaching rustc About Toylang Types

**File:** `src/queries/layout.rs`

**Purpose:** Every type that rustc touches must have a known size and alignment. When rustc
computes the layout of `Vec<Point>`, it calls `layout_of(Point)`. Without an override, this
would fail because `Point` is declared as `struct Point { x: i32, y: i32 }` in the Rust
source — rustc can compute that layout itself. But in the general case we want Toylang to
control layout, so we intercept and return our own.

**Query signature:**
```rust
fn toy_layout_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    query: PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, &'tcx LayoutError<'tcx>>
```

**Detection:** We match only `TyKind::Adt(...)` and check the ADT's name against the
registry. This is essential — not just `Point` but also `*mut Point`, `&mut Point`,
`FnDef(..., [Point])`, etc. all pass through `layout_of`. If we didn't filter by
`TyKind::Adt`, we'd corrupt pointer/reference layouts and get codegen ICEs.

```rust
let struct_name = REGISTRY.get().and_then(|reg| {
    if let TyKind::Adt(adt_def, _) = ty.kind() {
        let name = tcx.item_name(adt_def.did()).to_string();
        reg.structs.keys().find(|k| k.as_str() == name).cloned()
    } else {
        None
    }
});
```

**Layout construction:** We fill in a `LayoutData` struct (rustc's internal layout
representation) with the size, alignment, and field offsets computed by `ToyStruct`:

```rust
LayoutData {
    fields: FieldsShape::Arbitrary {
        offsets: [...],       // byte offset of each field (after padding)
        memory_index: [0,1,2,...],  // field order in memory
    },
    variants: Variants::Single { index: VariantIdx::from_u32(0) },
    backend_repr: BackendRepr::Memory { sized: true },
    largest_niche: None,
    align: AbiAndPrefAlign::new(align),
    size: Size::from_bytes(total_size),
    max_repr_align: None,
    unadjusted_abi_align: align,
    randomization_seed: 0,
}
```

`ToyStruct::size()` and `ToyStruct::field_offsets()` compute standard C struct layout:
align each field to its natural alignment, pad the struct total size to the max field
alignment.

**Result:** `TyAndLayout { ty, layout: tcx.mk_layout(layout_data) }`. The `mk_layout`
call interns the data into the arena so it can be stored behind a `'tcx` reference.

---

### Override 2: `mir_built` — Synthesizing Function Bodies

**File:** `src/queries/mir_build.rs`, `src/toylang/lower.rs`, `src/mir_helpers.rs`

**Purpose:** Every function body in a compilation unit must have a MIR `Body`. Normally
rustc constructs this by lowering the function's HIR (parsed Rust AST). For Toylang
functions, the Rust source contains `unreachable!()` stubs — we intercept the query and
return a completely synthesized `Body` instead.

**Query signature:**
```rust
fn toy_mir_built<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> &'tcx Steal<Body<'tcx>>
```

The return type is `&'tcx Steal<Body<'tcx>>` — a `Steal` is a wrapper that allows
one-time ownership transfer (later passes "steal" the body to transform it).

**Dispatch:**

```rust
if let Some(fn_name) = toylang_fn_name(tcx, def_id) {
    let body = if let Some(toy_fn) = registry.functions.get(&fn_name) {
        if let Some(fn_body) = &toy_fn.body {
            // AST-driven path: lower the parsed AST to MIR
            let param_names = toy_fn.params.iter().map(|p| p.name.clone()).collect();
            lower::build_body(tcx, def_id, &param_names, fn_body)
        } else {
            // No parsed body: use hardcoded builder (get_x returns 42)
            build_hardcoded(tcx, def_id, &fn_name)
        }
    };
    return tcx.arena.alloc(Steal::new(body));
}
// Not a Toylang function — delegate to rustc's original provider
DEFAULT_MIR_BUILT(tcx, def_id)
```

`toylang_fn_name` looks up the item's name and checks if it appears in the registry's
`functions` map. If yes, it's ours to handle.

---

### Override 3: `mir_borrowck` — Skipping Borrow Checking

**File:** `src/queries/borrowck.rs`

**Purpose:** rustc's borrow checker would reject our hand-built MIR bodies. They don't have
the borrow check annotations that the standard HIR lowerer produces. We skip it entirely for
Toylang items.

**Detection:**

```rust
fn is_toylang_item(tcx: TyCtxt<'_>, def_id: LocalDefId) -> bool {
    // Primary: file extension check (for when a .toylang file loader exists)
    let span = tcx.def_span(def_id);
    let file = tcx.sess.source_map().lookup_source_file(span.lo());
    if file.name.prefer_local().to_string().ends_with(".toylang") {
        return true;
    }
    // Fallback: name-based registry lookup (for stub functions in .rs files)
    if let Some(name) = tcx.opt_item_name(def_id.to_def_id()) {
        if let Some(registry) = REGISTRY.get() {
            return registry.functions.contains_key(name.as_str());
        }
    }
    false
}
```

Currently, Toylang functions are declared as stubs in `.rs` files (`tests/host.rs`), so
`def_span` points to a `.rs` file and the file-extension check fails. The name-based fallback
handles this. If a future file loader existed that fed synthetic `.toylang` source to rustc,
the file-extension check would take over.

**What we return:** An empty `BorrowCheckResult`:

```rust
tcx.arena.alloc(BorrowCheckResult {
    concrete_opaque_types: Default::default(),
    closure_requirements: None,
    used_mut_upvars: Default::default(),
    tainted_by_errors: None,
})
```

This is the "nothing to report" sentinel — exactly what a function with no borrows, no
closures, and no opaque types would return. Normal `.rs` functions are delegated to the
default `mir_borrowck`.

---

### Override 4: `mir_shims` — Drop Glue for Toylang Types

**File:** `src/queries/drop_glue.rs`, `src/mir_helpers.rs`

**Purpose:** When a `Vec<Point>` is dropped, Rust's drop glue calls
`drop_in_place::<Point>()`. The body for this synthetic function is produced by `mir_shims`
(not `mir_built`). We intercept it to emit a call to a Toylang-provided destructor.

**Query signature:**
```rust
fn toy_mir_shims<'tcx>(
    tcx: TyCtxt<'tcx>,
    instance: ty::InstanceKind<'tcx>,
) -> Body<'tcx>   // NOTE: returns Body, not &'tcx Steal<Body>
```

This is different from `mir_built`: `mir_shims` returns an owned `Body`, not a `Steal`.

**Detection:** We pattern-match on `InstanceKind::DropGlue(def_id, Some(ty))`:

```rust
if let ty::InstanceKind::DropGlue(def_id, Some(ty)) = instance {
    if let Some(struct_name) = toylang_struct_name(tcx, ty) {
        return build_drop_call_body(tcx, def_id, ty, &struct_name);
    }
}
```

The `Some(ty)` in the pattern is important. When a type has no `Drop` impl and no
droppable fields, rustc emits `DropGlue(def_id, None)` — a no-op. `Some(ty)` only appears
when the type genuinely needs dropping. To trigger this, `tests/drop_test.rs` declares:

```rust
impl Drop for Point {
    fn drop(&mut self) { unreachable!() }
}
```

This forces `DropGlue(def_id, Some(Point))` to be generated, which our override catches.
The stub `unreachable!()` body is never executed — our override replaces it.

**The synthesized body** (`build_drop_call_body` in `mir_helpers.rs`):

```
// Signature: fn(*mut Point) -> ()
bb0:
    _0 = __toylang_drop_Point(copy _1) → bb1;
bb1:
    return;
```

It finds `__toylang_drop_Point` by scanning `tcx.hir_crate_items(()).foreign_items()` for
the name, then builds a `TerminatorKind::Call` targeting it.

**Critical `mir_shims` rule:** Bodies returned from `mir_shims` must explicitly call
`body.set_required_consts(vec![])` and `body.set_mentioned_items(vec![])`. If not,
the monomorphization collector panics because those fields are unset. This is unique to
`mir_shims` — for `mir_built` bodies, the `mir_promoted` pass sets them automatically.

```rust
body.set_required_consts(vec![]);
body.set_mentioned_items(vec![]);
```

---

### Override 5: The Type Oracle (`src/oracle.rs`)

This isn't a query override but a support module that the MIR lowerer uses to resolve
the actual `DefId`s of standard library functions.

**`find_local_struct_ty`** — walks `tcx.hir_crate_items(()).definitions()` looking for a
struct with a matching name, returns its `Ty<'tcx>`:

```rust
for local_def_id in tcx.hir_crate_items(()).definitions() {
    let def_id = local_def_id.to_def_id();
    if tcx.def_kind(def_id) == DefKind::Struct {
        if tcx.item_name(def_id).as_str() == name {
            return Some(Ty::new_adt(tcx, tcx.adt_def(def_id), List::empty()));
        }
    }
}
```

**`find_vec_method`** — finds a named method in Vec's inherent impls. Vec has many impl
blocks; all must be searched:

```rust
let vec_def_id = tcx.get_diagnostic_item(sym::Vec)?;
for &impl_id in tcx.inherent_impls(vec_def_id) {
    for &item_id in tcx.associated_item_def_ids(impl_id) {
        if tcx.item_name(item_id).as_str() == method {
            return Some(item_id);
        }
    }
}
```

`sym::Vec` is a rustc built-in symbol constant that maps to `std::vec::Vec`'s `DefId`.

**`extract_global_ty`** — extracts the `Global` allocator type from `Vec::new`'s return type.
`Vec<T>` is really `Vec<T, Global>`. To build `push`/`len` calls we need the `Global` type:

```rust
let args = tcx.mk_args(&[ty::GenericArg::from(point_ty)]);
let sig = tcx.fn_sig(new_def_id).instantiate(tcx, args).skip_binder();
// sig.output() = Vec<Point, Global>
if let TyKind::Adt(_, adt_args) = sig.output().kind() {
    Some(adt_args[1].expect_ty())  // adt_args[0]=Point, adt_args[1]=Global
}
```

---

## Part 5: MIR — What It Is and How We Build It

### MIR Concepts

MIR (Mid-level Intermediate Representation) is a control-flow graph. A `Body<'tcx>` has:

```
Body {
    basic_blocks: IndexVec<BasicBlock, BasicBlockData>
    local_decls:  IndexVec<Local, LocalDecl>
    arg_count:    usize
    ...
}
```

**Locals:** `Local(0)` (`_0`) is always the return place. `Local(1)` through
`Local(arg_count)` are function arguments, in order. All other locals are temporaries.

**Basic blocks:** Each `BasicBlockData` contains:
- `statements: Vec<Statement>` — side-effecting but non-branching operations
- `terminator: Terminator` — how control leaves the block

**Key statement kinds:**
- `StatementKind::Assign(place, rvalue)` — `_1 = some_value`
- `StatementKind::StorageLive(local)` — announce that a local's storage is being used
- `StatementKind::StorageDead(local)` — announce that a local's storage is no longer used

**Key terminator kinds:**
- `TerminatorKind::Return` — return from the function
- `TerminatorKind::Goto { target }` — unconditional branch
- `TerminatorKind::Call { func, args, destination, target, ... }` — function call;
  stores return value in `destination`, continues at `target`

**Key rvalue kinds:**
- `Rvalue::Use(operand)` — copy or move a value
- `Rvalue::Ref(region, borrow_kind, place)` — take a reference
- `Rvalue::Aggregate(kind, operands)` — construct a struct, tuple, or array

**Key operand kinds:**
- `Operand::Copy(place)` — copy from a place (requires the type to be `Copy`)
- `Operand::Move(place)` — move from a place
- `Operand::Constant(const_operand)` — a compile-time constant

### MIR Validity Rules

`-Zvalidate-mir` enforces these rules on every `Body`:

1. **Every local except `_0` and argument locals must have exactly one `StorageLive` before
   first use and a `StorageDead` after last use.** If StorageLive/Dead are missing or in
   the wrong block, you'll get validation errors.

2. **`StorageDead` for a local used as a call argument must go in the *successor* block,
   not the block containing the `Call` terminator.** The call terminator ends the block;
   the local's storage is still live until the successor block.

3. **The type of `Local(0)` must exactly match `tcx.fn_sig(def_id).skip_binder().output()`.**

4. **Every `BasicBlockData` must have `terminator: Some(...)`.**

5. **The `arg_count` field must match the number of argument locals (and `fn_sig.inputs().len()`).**

6. **One `SourceScopeData` is required** at index 0 (the outermost scope). Its `span` should
   be the function's definition span.

### Why Not Set `required_consts` / `mentioned_items` in `mir_built` Bodies?

These two fields are set by the `mir_promoted` pass, which runs on `mir_built` output. If
you pre-set them in `build_body`, the promoted pass finds them already set and panics with
`"required_consts have already been set"`. Leave them unset for `mir_built` bodies.

For `mir_shims` bodies (drop glue), `mir_promoted` does *not* run. The monomorphization
collector reads them directly. If unset, it panics. So `mir_shims` bodies *must* set them:

```rust
body.set_required_consts(vec![]);
body.set_mentioned_items(vec![]);
```

---

## Part 6: The MIR Lowerer

**File:** `src/toylang/lower.rs`

`lower::build_body` is the central new piece of the system. It takes a parsed `FnBody` AST
and emits a valid rustc `Body<'tcx>`.

### Initialization

At the start of `build_body`, we resolve all the types we'll need:

```rust
let elem_ty      = find_local_struct_ty(tcx, "Point")        // Point
let new_def_id   = find_vec_method(tcx, "new")               // Vec::new
let push_def_id  = find_vec_method(tcx, "push")              // Vec::push
let len_def_id   = find_vec_method(tcx, "len")               // Vec::len
let global_ty    = extract_global_ty(tcx, elem_ty, new_def_id) // Global

// Vec<Point, Global> — from Vec::new's return type
let vec_ty = tcx.fn_sig(new_def_id).instantiate(tcx, new_args).skip_binder().output()

// &mut Vec<Point, Global>
let vec_mut_ref_ty = Ty::new_mut_ref(tcx, tcx.lifetimes.re_erased, vec_ty)
```

Return type and argument types come from `tcx.fn_sig(def_id)` — not from the `.toylang`
source's type strings. This guarantees the locals match what rustc expects.

### Local Allocation

```rust
// _0 = return place
locals.push(LocalDecl::new(ret_ty, span));

// _1, _2, ... = function arguments (from fn_sig.inputs(), named by registry params)
for (name, &input_ty) in param_names.iter().zip(fn_sig.inputs().iter()) {
    let local = locals.push(LocalDecl::new(input_ty, span));
    local_map.insert(name.clone(), local);
}
```

After this, `alloc_local(ty)` appends temporaries to `locals` and returns their index.

### The Lowering State (`Lower`)

```rust
struct Lower<'tcx> {
    tcx: TyCtxt<'tcx>,
    span: Span,
    source_info: SourceInfo,
    locals: IndexVec<Local, LocalDecl<'tcx>>,
    local_map: HashMap<String, Local>,   // variable name → local index
    blocks: IndexVec<BasicBlock, BasicBlockData<'tcx>>,
    current_stmts: Vec<Statement<'tcx>>, // statements accumulating for the current block
    elem_ty, global_ty, new_def_id, push_def_id, len_def_id,
    vec_ty, vec_mut_ref_ty,
}
```

`current_stmts` is the "current block in progress." When a `Call` terminator is emitted,
`finalize_block` drains `current_stmts` into a `BasicBlockData` and starts fresh.

### `finalize_block` — Block Boundary Management

Every `Call` in MIR *ends* a basic block. The call is the terminator; the next statement
(after the call returns) begins a new block. `finalize_block` handles this:

```rust
fn finalize_block(&mut self, terminator: Terminator<'tcx>) -> BasicBlock {
    let stmts = std::mem::take(&mut self.current_stmts);
    self.blocks.push(BasicBlockData { statements: stmts, terminator: Some(terminator), ... });
    BasicBlock::from_u32(self.blocks.len() as u32)  // index of the next (unstarted) block
}
```

When we emit a `Call` terminator, we need to know the `target` block number *before* we
create it. We compute it as `blocks.len() + 1` (before finalize adds the current block):

```rust
// Before finalize: blocks.len() = N
// After finalize: blocks.len() = N+1 (current block is now at index N)
// Next block (target) will be at index N+1
let next_bb = BasicBlock::from_u32(self.blocks.len() as u32 + 1);
let term = Terminator { kind: TerminatorKind::Call { target: Some(next_bb), ... }, ... };
self.finalize_block(term);
// Now we're in the new block (at index N+1)
```

### Lowering Statements

**`Stmt::Let { name, expr }`:**
```
alloc local of inferred type
register name → local in local_map
emit StorageLive(local)
lower_expr_into(expr, Place::from(local))
```

**`Stmt::ExprStmt(expr)`:**
```
alloc temp of inferred type (usually unit for push)
emit StorageLive(temp)
lower_expr_into(expr, Place::from(temp))
// If expr was a call, we're now in the successor block:
emit StorageDead(temp)
```

### Lowering Expressions into a Destination Place

**`Expr::IntLit(n)`:**
```
emit Assign(dest, Use(Constant(i32 scalar n)))
```

**`Expr::Var(name)`:**
```
local = local_map[name]
emit Assign(dest, Use(Move(Place::from(local))))
```
Uses `Move` (not `Copy`) because `Vec<Point>` is not `Copy`. Move works for both.

**`Expr::StructLit { name, fields }`:**
```
for each (field_name, field_expr):
    alloc field_tmp of field_expr's type
    emit StorageLive(field_tmp)
    lower_expr_into(field_expr, Place::from(field_tmp))
emit Assign(dest, Aggregate(Adt { did, variant=0, args=[] }, [Move(tmp)...]))
for each field_tmp: emit StorageDead(field_tmp)
```

The `AggregateKind::Adt` constructor takes:
- `did` — the struct's `DefId`
- `variant_idx` — always 0 for non-enum structs
- `tcx.mk_args(&[])` — generic args (empty for a non-generic struct like Point)

**`Expr::StaticCall { ty: "Vec", method: "new", args: [] }`:**
```
func = Constant(FnDef(new_def_id, [Point]))
next_bb = blocks.len() + 1
finalize_block(Call { func, args: [], destination: dest, target: next_bb })
```

**`Expr::MethodCall { receiver, method: "push", args: [arg] }`:**
```
recv_local = local_map[receiver_var_name]
alloc ref_local of &mut Vec type
emit StorageLive(ref_local)
emit Assign(ref_local, Ref(&mut, recv_local))    // &mut recv_local
alloc arg_local of arg's type
emit StorageLive(arg_local)
lower_expr_into(arg, Place::from(arg_local))
next_bb = blocks.len() + 1
finalize_block(Call { func: push, args: [Move(ref_local), Move(arg_local)], dest, target: next_bb })
// Now in successor block:
emit StorageDead(arg_local)
emit StorageDead(ref_local)
```

`push` takes `&mut self` — so we must pass a `&mut Vec<Point>` as the first argument, not
the `Vec` local directly. The borrow (`Ref`) must happen in the pre-call block.

**`Expr::MethodCall { receiver, method: "len", args: [] }`:**
```
recv_local = local_map[receiver_var_name]   // already &Vec (from fn param)
next_bb = blocks.len() + 1
finalize_block(Call { func: len, args: [Copy(recv_local)], dest, target: next_bb })
```

`len` takes `&self` — the argument is already `&Vec<Point>` (it's a function parameter),
so we `Copy` it directly with no additional borrow needed.

### Type Inference (Shallow)

The lowerer uses a simple `infer_ty` function to determine what type a local should have:

```rust
fn infer_ty(&self, expr: &Expr) -> Ty<'tcx> {
    match expr {
        IntLit(_)                           => tcx.types.i32,
        Var(name)                           => locals[local_map[name]].ty,
        StaticCall { ty: "Vec", .. }        => vec_ty,
        MethodCall { method: "push", .. }   => tcx.types.unit,
        MethodCall { method: "len", .. }    => tcx.types.usize,
        StructLit { .. }                    => elem_ty,
    }
}
```

This works for the current grammar. It would need to be extended for new operations.

### The Complete `make_vec` MIR

For the input:
```toylang
fn make_vec() -> Vec<Point> {
    let v = Vec::new();
    v.push(Point { x: 1, y: 2 });
    v.push(Point { x: 3, y: 4 });
    v
}
```

The lowerer produces roughly this MIR (local indices will vary):

```
// Locals:
// _0: Vec<Point, Global>     — return place
// _1: Vec<Point, Global>     — v
// _2: ()                     — ExprStmt temp (first push result)
// _3: &mut Vec<Point, Global>— push receiver borrow
// _4: Point                  — push argument
// _5: i32, _6: i32           — Point field temps
// _7: ()                     — ExprStmt temp (second push)
// _8: &mut Vec<Point, Global>
// _9: Point, _10: i32, _11: i32

bb0:
    StorageLive(_1);
    // Call Vec::new, store into _1
    _1 = Vec::<Point>::new() → bb1;

bb1:
    StorageLive(_2);
    StorageLive(_3);
    _3 = &mut _1;
    StorageLive(_4);
    StorageLive(_5);
    _5 = const 1_i32;
    StorageLive(_6);
    _6 = const 2_i32;
    _4 = Point { x: move _5, y: move _6 };
    StorageDead(_6); StorageDead(_5);
    _2 = Vec::<Point>::push(move _3, move _4) → bb2;

bb2:
    StorageDead(_4); StorageDead(_3); StorageDead(_2);
    StorageLive(_7);
    StorageLive(_8);
    _8 = &mut _1;
    StorageLive(_9);
    StorageLive(_10); _10 = const 3_i32;
    StorageLive(_11); _11 = const 4_i32;
    _9 = Point { x: move _10, y: move _11 };
    StorageDead(_11); StorageDead(_10);
    _7 = Vec::<Point>::push(move _8, move _9) → bb3;

bb3:
    StorageDead(_9); StorageDead(_8); StorageDead(_7);
    _0 = move _1;   // trailing return expr
    return;
```

### The Complete `vec_len` MIR

```toylang
fn vec_len(v: &Vec<Point>) -> usize {
    v.len()
}
```

```
// Locals:
// _0: usize                  — return place
// _1: &Vec<Point, Global>    — v (argument)

bb0:
    _0 = Vec::<Point>::len(copy _1) → bb1;

bb1:
    return;
```

---

## Part 7: End-to-End Compilation Walkthrough

When you run:
```bash
./target/debug/toylangc --edition 2021 --toylang-input tests/point.toylang tests/host.rs -o /tmp/out
```

Here is what happens in order:

**1. Argument parsing (`main.rs`):**
`--toylang-input tests/point.toylang` is extracted. `point.toylang` is read and parsed into
a `ToylangRegistry` containing `Point` (struct) and `make_vec`/`vec_len` (functions with
parsed `FnBody`). The flags are stripped from `args`.

**2. `RunCompiler::new(&args, &mut ToyCallbacks).run()` begins.**

**3. `ToyCallbacks::config` is called:**
The registry is installed into four `OnceLock` statics (one per query module).
`config.override_queries = Some(toy_override_queries)` is set.

**4. `toy_override_queries` is called:**
The four default providers are saved; our four overrides are installed into `Providers`.

**5. rustc parses `host.rs`:**
It sees `struct Point { x: i32, y: i32 }`, `fn make_vec() -> Vec<Point>`,
`fn vec_len(v: &Vec<Point>) -> usize`, and `fn main()`. All are legal Rust. The stub
bodies (`unreachable!()`) are syntactically valid.

**6. HIR lowering and name resolution proceed normally.**

**7. Type checking begins. rustc needs `layout_of(Point)`:**
Our `toy_layout_of` intercepts. It matches `TyKind::Adt` with name `"Point"`,
builds a `LayoutData` (8 bytes, 4-byte aligned, two i32 fields at offsets 0 and 4),
returns `TyAndLayout`. rustc can now compute `size_of::<Vec<Point>>()`.

**8. `mir_built(make_vec)` is called:**
`toy_mir_built` detects `"make_vec"` is in the registry. It finds `body: Some(fn_body)`.
It calls `lower::build_body(tcx, def_id, &[], fn_body)`. The lowerer resolves types,
allocates locals, lowers three statements and a trailing expression into four basic blocks,
emits `Vec::new` and two `Vec::push` calls. Returns `Steal::new(body)`.

**9. `mir_built(vec_len)` is called:**
Similarly intercepted. `param_names = ["v"]`. Local `_1` is registered in `local_map`
as `v`. The body lowers `v.len()` as the trailing expression: one basic block with a
`Vec::len` call into `_0` (return place), then Return.

**10. `mir_borrowck(make_vec)` and `mir_borrowck(vec_len)` are called:**
`is_toylang_item` returns `true` (name-based lookup). Empty `BorrowCheckResult` returned.
No borrow errors.

**11. MIR optimization passes run on all four functions** (including `main`).
Our synthesized bodies go through the standard optimization pipeline like any other MIR.

**12. Monomorphization.** rustc finds `Vec<Point>::push` is called, instantiates it with
`T=Point`. This triggers `layout_of(Point)` again — intercepted as before. Also triggers
`drop_in_place::<Point>()` (because `Vec` drops its elements).

**13. `mir_shims(DropGlue(_, Some(Point)))` is called:**
`toy_mir_shims` detects the type is `Point` (a Toylang type). It builds a body that
calls `__toylang_drop_Point`. Sets `required_consts` and `mentioned_items` to empty.
Returns the owned `Body`.

**14. Codegen.** LLVM receives all the MIR, including our synthesized bodies, generates
machine code, links, produces `/tmp/out`.

**15. Running `/tmp/out`:**
`main` calls `make_vec()` → two Points are pushed → `vec_len(&v)` → `Vec::len` returns 2
→ "Vec length: 2" prints → `v` goes out of scope → drop glue runs (but `Point` doesn't
actually need dropping since it has no Drop impl in this test) → exits.

---

## Part 8: The Test Suite

### `tests/host.rs` — Main End-to-End Test

Declares `struct Point`, `fn make_vec`, `fn vec_len` as Rust stubs. Calls them from `main`.
Verifies `Vec length: 2`. This exercises the complete pipeline: layout, mir_built for both
functions, borrowck skip.

Compile and run:
```bash
DYLD_LIBRARY_PATH=$(rustup run nightly-2025-01-15 rustc --print=sysroot)/lib \
  ./target/debug/toylangc --edition 2021 --toylang-input tests/point.toylang \
  tests/host.rs -o /tmp/host_test
/tmp/host_test
# Vec length: 2
```

### `tests/mir_test.rs` — Hardcoded MIR Regression Test

Uses `get_x(_p: Point) -> i32` with `body: None` in the registry (hardcoded fallback).
Verifies `get_x = 42`. This exercises the fallback path in `mir_build.rs` and verifies
the `hardcoded_point()` registry still works without a `.toylang` file.

```bash
DYLD_LIBRARY_PATH=... ./target/debug/toylangc --edition 2021 tests/mir_test.rs -o /tmp/mir_test
/tmp/mir_test
# get_x = 42
```

### `tests/drop_test.rs` — Drop Glue Test

Declares `Point` with `impl Drop { fn drop(&mut self) { unreachable!() } }`. The `unreachable!`
body is replaced by the `mir_shims` override. Declares `extern "C" __toylang_drop_Point`.
Manually calls `drop_in_place` and verifies the runtime function fires.

```bash
DYLD_LIBRARY_PATH=... ./target/debug/toylangc --edition 2021 tests/drop_test.rs \
  -C link-arg=tests/runtime.o -o /tmp/drop_test
/tmp/drop_test
# [toylang] dropping Point at 0x...
# done
```

### MIR Validation

```bash
RUSTFLAGS="-Zvalidate-mir" DYLD_LIBRARY_PATH=... \
  ./target/debug/toylangc --edition 2021 --toylang-input tests/point.toylang \
  tests/host.rs -o /tmp/host_validated
# Should produce no validation errors
```

---

## Part 9: Gotchas and Lessons Learned

### `layout_of` must filter by `TyKind::Adt`

The `layout_of` query is called for *every* type rustc encounters during compilation —
not just `Point` itself, but `*mut Point`, `&mut Point`, `Option<Point>`,
`FnDef(push, [Point, Global])`, and so on. If you match by name alone (e.g., checking
if the debug representation contains "Point"), you'll accidentally intercept these derived
types, corrupt their layouts, and get ICEs in codegen. Always check `TyKind::Adt` first.

### `mir_shims` bodies require `set_required_consts` and `set_mentioned_items`

`mir_built` bodies: do NOT set these. The `mir_promoted` pass sets them.
`mir_shims` bodies: MUST set these. The monomorphization collector reads them directly.
Getting this backwards causes a "have already been set" panic or a monomorphization ICE.

### `DropGlue(_, None)` vs `DropGlue(_, Some(T))`

`None` means "no-op drop" — emitted for types with no `Drop` impl and no droppable fields.
`Some(T)` is only emitted when the type genuinely needs dropping. To test drop interception,
the type *must* have a `Drop` impl (even a stub with `unreachable!()`).

### `StorageDead` belongs in the successor block

When a local is used as an argument to a `Call` terminator, its `StorageDead` must be
emitted *after* the call — i.e., in the successor block. The `Call` terminator ends the
block; the local's storage is live across the call boundary.

### Query providers cannot be closures

`override_queries` is `fn(&Session, &mut Providers)` — a plain function pointer. Closures
that capture state cannot be stored in function pointers. All shared state (the registry,
the default providers) must go through `OnceLock` statics.

### `Steal<Body>` vs `Body`

`mir_built` returns `&'tcx Steal<Body<'tcx>>` — the body is arena-allocated and wrapped
in `Steal` to allow one-time ownership transfer by later passes.
`mir_shims` returns `Body<'tcx>` (owned, not in a `Steal`) — rustc handles interning it.

### `DoubleColon` before `Colon` in the lexer

The lexer must check for `::` before checking for `:`. Without this, `Vec::new` would be
lexed as `Vec`, `:`, `:`, `new` — which would fail to parse as a static call.

### Using `OnceLock` instead of `thread_local!`

rustc executes some queries on Rayon worker threads. `thread_local!` statics are per-thread
— they would appear unset on worker threads. `OnceLock` is global and set-once; safe for
any thread to read after initialization.

---

## Part 10: Debugging

### Useful Environment Variables

```bash
# Dump MIR for all functions after overrides are applied
RUSTFLAGS="-Zdump-mir=all" ./target/debug/toylangc ...

# Enable MIR validation — reports structural errors in synthesized bodies
RUSTFLAGS="-Zvalidate-mir" ./target/debug/toylangc ...

# Show the HIR — useful for understanding how rustc sees the Rust side
RUSTFLAGS="-Zunpretty=hir-tree" ./target/debug/toylangc ...

# Trace specific queries (very verbose)
RUSTC_LOG=rustc_query_system=debug ./target/debug/toylangc ... 2>&1 | grep layout_of

# Enable the oracle dump
TOYLANG_DUMP_TYPES=1 ./target/debug/toylangc ...
```

### Common Errors and Their Causes

| Error | Likely Cause |
|-------|--------------|
| ICE: unexpected type during codegen | `layout_of` is intercepting pointer/reference types; add `TyKind::Adt` guard |
| `required_consts have already been set` | Called `set_required_consts` on a `mir_built` body; remove it |
| Monomorphization ICE: `required_consts` unset | Forgot `set_required_consts` on a `mir_shims` body; add it |
| `StorageLive(_N) not found` | Missing `StorageLive` before first use of a local |
| Return type mismatch | `Local(0)` type doesn't match `tcx.fn_sig(def_id).output()` |
| `unknown function 'make_vec'` | Parser not wired up; `body: None` for a function that needs lowering |
| `undefined variable 'v'` | Param names not registered in `local_map` (missing arg-local setup) |
| `default mir_built not saved` | `save_default` called after `override_queries`; order matters |

### Confirming Your Override Fires

Each override logs to stderr:

```
[toylang] layout_of intercepted for: Point
[toylang] mir_built intercepted for: make_vec
[toylang] mir_built intercepted for: vec_len
[toylang] mir_shims/DropGlue intercepted for: Point
```

If you don't see these, the override isn't matching. Check:
- Is the function in the `functions` map?
- Is the struct in the `structs` map?
- Is the registry set before queries run?

---

## Part 11: Extension Points

The current implementation is intentionally narrow — it handles `Point`, `Vec::new`,
`Vec::push`, and `Vec::len`. Here's where the code would grow to support more:

**New Toylang types:** `find_local_struct_ty` and `lower.rs` currently hardcode `"Point"`.
To generalize, `lower.rs` would need to look up the struct name from the `StructLit` node
against the registry and call `find_local_struct_ty` dynamically.

**New operations:** `infer_ty` and `lower_expr_into` would need new match arms for each
new expression form (arithmetic, comparisons, field access, etc.).

**Nested struct types:** `ToyFieldType` currently only supports primitives. Supporting
`ToyFieldType::ToyStruct(String)` would require recursive layout computation and recursive
lowering for struct construction.

**Control flow:** The AST and lowerer have no `if`/`while`/`loop`. Adding them would
require the lowerer to allocate labeled basic blocks, emit `SwitchInt` terminators, and
handle phi-like joins.

**Type checking:** `src/toylang/typeck.rs` (referenced in the project structure but not
yet implemented) would be a pass over the AST that verifies type correctness before
lowering — catching errors like passing a `Vec` where a `Point` is expected.

**File loader:** Currently Toylang functions are declared as Rust stubs (`unreachable!()`)
in `.rs` files. A custom `FileLoader` could synthesize these Rust stubs on-the-fly from
`.toylang` definitions, making `host.rs` unnecessary for declarations.
