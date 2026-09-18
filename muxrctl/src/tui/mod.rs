//! TUI layer: terminal lifecycle, event loop, theme, and rendering.
//!
//! Depends on [`crate::app`] (the pure TEA core) but not vice-versa: all
//! ratatui / crossterm / terminal I/O lives here. [`views`] owns the one
//! dashboard and every dialog declared over it.

pub mod components;
pub mod runner;
pub mod terminal;
pub mod theme;
pub mod views;
pub mod widgets;
