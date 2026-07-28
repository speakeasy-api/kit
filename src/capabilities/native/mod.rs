//! The complete eager, model-facing native coding surface.
//!
//! Descriptors are public for inspection. Dispatch remains crate-private so an
//! external consumer cannot manufacture authority around the M001 kernel.

mod catalog;
pub(crate) mod dispatch;
pub(crate) mod orchestrate;

pub use catalog::{
    JSON_SCHEMA_DIALECT, MAX_NATIVE_INPUT_BYTES, MAX_NATIVE_OUTPUT_BYTES, NativeCatalog,
    NativeTool, NativeToolDescriptor,
};
