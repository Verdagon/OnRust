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
            offset = (offset + align - 1) & !(align - 1);
            offset += field.rust_type.size();
        }
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
    /// Replace this with a real parser in Step 10.
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
            params: vec![],
            return_ty: Some("Vec<Point>".to_string()),
            body: None,
        });
        functions.insert("vec_len".to_string(), ToyFunction {
            name: "vec_len".to_string(),
            params: vec![ToyParam { name: "v".to_string(), ty: "&Vec<Point>".to_string() }],
            return_ty: Some("usize".to_string()),
            body: None,
        });
        functions.insert("get_x".to_string(), ToyFunction {
            name: "get_x".to_string(),
            params: vec![],
            return_ty: None,
            body: None,
        });

        Self { structs, functions }
    }

    pub fn is_toylang_type(&self, name: &str) -> bool {
        self.structs.contains_key(name)
    }
}

/// A parsed parameter in a Toylang function signature.
#[derive(Clone, Debug)]
pub struct ToyParam {
    pub name: String,
    pub ty: String,
}

#[derive(Clone, Debug)]
pub struct ToyFunction {
    pub name: String,
    pub params: Vec<ToyParam>,
    pub return_ty: Option<String>,
    pub body: Option<crate::toylang::ast::FnBody>,
}
