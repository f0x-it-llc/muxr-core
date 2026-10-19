//! Custom ratcn components muxrctl needs beyond the built-ins.
//!
//! `ratcn` ships no text input, so [`text_input`] fills that gap; the Config,
//! token-create and wizard views declare `TextInput` the way they would any
//! other ratcn component. [`shortcuts`] wraps every dialog so its letters work,
//! and [`action`] gives every dialog button the one shared look.

pub mod action;
pub mod shortcuts;
pub mod text_input;
pub use shortcuts::DialogShortcuts;
pub use text_input::TextInput;
