/// A Toylang expression.
#[derive(Clone, Debug)]
pub enum Expr {
    IntLit(i64),
    Var(String),
    /// `Vec::new()` — IDENT "::" IDENT "(" args ")"
    StaticCall { ty: String, method: String, args: Vec<Expr> },
    /// `v.push(x)` — expr "." IDENT "(" args ")"
    MethodCall { receiver: Box<Expr>, method: String, args: Vec<Expr> },
    /// `Point { x: 1, y: 2 }` — IDENT "{" field_inits "}"
    StructLit { name: String, fields: Vec<(String, Expr)> },
}

/// A Toylang statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    Let { name: String, expr: Expr },
    ExprStmt(Expr),
}

/// A parsed Toylang function body.
#[derive(Clone, Debug)]
pub struct FnBody {
    pub stmts: Vec<Stmt>,
    pub ret: Option<Expr>, // trailing expression — becomes return value
}
