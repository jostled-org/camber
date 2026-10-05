//! Build helpers for Camber code generation.
//!
//! Use this crate from `build.rs` to compile `.proto` files and generate the
//! service glue expected by Camber's gRPC support.
//!
//! Tonic's own server and client code is generated for every RPC form. Unary
//! services also get an async `{service}_service` convenience wrapper; a
//! service with a streaming method gets an empty, documented module in its
//! place. See [`compile_protos`].

mod builder;
mod codegen;

pub use builder::{Builder, compile_protos, configure};
