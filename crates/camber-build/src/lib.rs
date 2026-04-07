//! Build helpers for Camber code generation.
//!
//! Use this crate from `build.rs` to compile `.proto` files and generate the
//! service glue expected by Camber's gRPC support.

mod builder;
mod codegen;

pub use builder::{Builder, compile_protos, configure};
