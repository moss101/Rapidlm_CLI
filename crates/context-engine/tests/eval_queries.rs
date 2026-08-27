//! Test-side registration of the gold-query runner.
//!
//! The runner lives in `benches/eval_queries.rs` and is also executed under
//! `cargo test` (harness-free) so its metric invariants run in CI. Cargo
//! warns when one source file belongs to multiple build targets, and
//! `include!` cannot carry the file's inner attributes, so this shim loads
//! it as a module instead.
#[path = "../benches/eval_queries.rs"]
mod eval_queries;

fn main() {
    eval_queries::main()
}
