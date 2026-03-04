#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_session;

mod callbacks;
mod queries;
mod oracle;
mod toylang;
mod mir_helpers;

use std::sync::Arc;
use rustc_driver::RunCompiler;
use crate::toylang::registry::ToylangRegistry;

fn main() {
    rustc_driver::install_ice_hook(
        "https://github.com/your-org/toylang/issues",
        |_| {},
    );

    let exit_code = rustc_driver::catch_with_exit_code(|| {
        let mut args: Vec<String> = std::env::args().collect();
        let registry = Arc::new(extract_registry(&mut args));
        RunCompiler::new(&args, &mut callbacks::ToyCallbacks::new(registry)).run();
        Ok(())
    });

    std::process::exit(exit_code);
}

fn extract_registry(args: &mut Vec<String>) -> ToylangRegistry {
    if let Some(pos) = args.iter().position(|a| a == "--toylang-input") {
        if pos + 1 < args.len() {
            let path = args[pos + 1].clone();
            args.drain(pos..=pos + 1);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("toylang: cannot read {}: {}", path, e));
            return crate::toylang::parser::parse(&src)
                .unwrap_or_else(|e| panic!("toylang: parse error in {}: {}", path, e));
        }
    }
    ToylangRegistry::hardcoded_point()
}
