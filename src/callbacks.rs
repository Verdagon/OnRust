#![allow(unused)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;

use rustc_driver::Compilation;
use rustc_interface::Config;
use rustc_middle::ty::TyCtxt;
use std::sync::Arc;

use crate::toylang::registry::ToylangRegistry;

pub struct ToyCallbacks {
    /// Parsed Toylang definitions, loaded before the rustc session starts.
    /// Arc because query providers need to access it from within closures
    /// passed into the rustc session.
    registry: Arc<ToylangRegistry>,
}

impl ToyCallbacks {
    pub fn new(registry: Arc<ToylangRegistry>) -> Self {
        Self { registry }
    }
}

impl rustc_driver::Callbacks for ToyCallbacks {
    fn config(&mut self, config: &mut Config) {
        crate::queries::layout::install_registry(self.registry.clone());
        crate::queries::borrowck::install_registry(self.registry.clone());
        crate::queries::mir_build::install_registry(self.registry.clone());
        crate::queries::drop_glue::install_registry(self.registry.clone());
        config.override_queries = Some(crate::queries::toy_override_queries);
    }

    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &rustc_interface::interface::Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        if std::env::var("TOYLANG_DUMP_TYPES").is_ok() {
            crate::oracle::dump_toylang_oracle(tcx, &self.registry);
        }
        Compilation::Continue
    }
}
