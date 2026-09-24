//! The `wikitui` binary: a thin shim over the library crate's entry point.
//! Everything — every module, the startup path, and the event loop — lives
//! in `src/lib.rs`; the crate is split into a library plus this shim only so
//! `benches/` can link the internals (PRD §9's criterion benches for §6.8).

fn main() -> anyhow::Result<()> {
    wikitui::main()
}
