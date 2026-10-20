//! Quorum session and command registry: every user-visible action is a command.
//!
//! [`Session::execute`] runs a [`CommandSpec`] by id with JSON params under the panic guard.
//! Menus, shortcuts, the palette, the CLI, the control channel and MCP all go through it.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

pub mod account;
mod cmd;
pub mod command;
mod error;
pub mod guard;
pub mod registration;
pub mod runtime;
mod session;

/// Settings, cached blobs and URL checks shared by the shell and the CLI.
pub use cmd::settings;
#[doc(inline)]
pub use command::{CommandSpec, Registry};
#[doc(inline)]
pub use error::{Error, ErrorKind, Result};
#[doc(inline)]
pub use session::{Session, SessionOptions};
