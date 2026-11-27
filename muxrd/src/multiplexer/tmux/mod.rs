//! The tmux backend drives an unmodified tmux binary through its command line,
//! control mode and a pty-attached client.

pub mod agent_state;
pub mod attach;
pub mod backend;
pub mod commands;
pub mod control;
pub mod layout;
pub mod paths;
pub mod status;
pub mod watch;
