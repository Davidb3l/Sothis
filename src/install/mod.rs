//! The staged, verified install engine behind `sothis install|update|uninstall|rollback`.

pub mod cli;
pub mod engine;
#[cfg(test)]
mod engine_tests;
pub mod extract;
pub mod fsx;
pub mod layout;
pub mod legacy;
pub mod lock;
pub mod packaged;
pub mod pathenv;
pub mod place;
pub mod plugins;
pub mod proc;
pub mod receipt;
pub mod source;
pub mod verify;
