//! Library facade so benchmarks (and future integration tests) can access
//! internal modules of the binary crate. The binary entry point remains
//! `src/main.rs`.
pub mod mux;
mod utils;
