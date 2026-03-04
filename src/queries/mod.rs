extern crate rustc_middle;
extern crate rustc_session;

pub mod borrowck;
pub mod drop_glue;
pub mod layout;
pub mod mir_build;

/// Called from ToyCallbacks::config. Saves original providers then installs
/// our overrides. Must be a plain function pointer (not a closure) because
/// Config::override_queries is fn(...), not Box<dyn Fn(...)>.
pub fn toy_override_queries(
    _session: &rustc_session::Session,
    providers: &mut rustc_middle::util::Providers,
) {
    layout::save_default(providers.layout_of);
    borrowck::save_default(providers.mir_borrowck);
    mir_build::save_default(providers.mir_built);
    drop_glue::save_default(providers.mir_shims);

    providers.layout_of    = layout::toy_layout_of;
    providers.mir_borrowck = borrowck::toy_mir_borrowck;
    providers.mir_built    = mir_build::toy_mir_built;
    providers.mir_shims    = drop_glue::toy_mir_shims;
}
