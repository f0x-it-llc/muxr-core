//! Custom ratcn components muxrctl needs beyond the built-ins.
//!
//! `ratcn` ships no text input, so [`text_input`] fills that gap; screens
//! declare `TextInput` the same way they would any other ratcn component.
//! No screen wires one in yet — that lands in a later wave — so the
//! re-export is unused for now rather than churned back in when it is.

pub mod text_input;
#[allow(unused_imports)]
pub use text_input::TextInput;
