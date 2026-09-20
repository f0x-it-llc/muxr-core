//! Application layer (TEA: state / message / update / action).
//!
//! This layer is deliberately free of the drawing, terminal-backend and
//! async-runtime crates — it is the pure core that [`crate::tui`] drives and
//! renders. It does name the ratcn runtime's own plain state and event types
//! (`FocusState`, `ModalState`, `ToasterState`, `KeyEvent`), which carry no
//! terminal I/O of their own.

pub mod action;
pub mod message;
pub mod state;
pub mod update;

pub use action::UpdateAction;
pub use message::{Message, UiMsg};
pub use state::AppState;
pub use update::update;
