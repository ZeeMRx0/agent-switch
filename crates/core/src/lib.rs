//! agent-switch core: config, vault, safe writes, locks, state and the `Surface` trait that the adapter
//! phases implement.

pub mod adapters;
pub mod apfs_clone;
pub mod config;
pub mod doctor;
pub mod error;
pub mod fsutil;
pub mod helper_keychain;
pub mod keychain;
pub mod locks;
pub mod macos_apps;
pub mod paths;
pub mod policy;
pub mod proc;
pub mod providers;
pub mod security_cli;
pub mod shallow_home;
pub mod signing;
pub mod state;
pub mod surface;
pub mod switch;
pub mod token_refresh;
pub mod usage;
pub mod vault;
pub mod watcher;

pub use error::{Error, Result};
