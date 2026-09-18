//! TUI layer: terminal lifecycle, event loop, theme, and screen rendering.
//!
//! Depends on [`crate::app`] (the pure TEA core) but not vice-versa: all
//! ratatui / crossterm / terminal I/O lives here.

pub mod components;
pub mod runner;
pub mod screens;
pub mod terminal;
pub mod theme;
pub mod widgets;
