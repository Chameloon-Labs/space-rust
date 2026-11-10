//! Server SDK for Chameloon spaces.
//!
//! A [`Server`] serves a folder with one sub-folder per space (`space.json`, `pages/*.oui`, optional `img/`).
//! It knows nothing about HTTP libraries: give it an [`http::Request`] and send back the [`http::Response`].
//! The `serve` feature adds a small bundled HTTP server (see [`serve`]).
mod auth;
#[cfg(feature = "serve")]
mod serve;
mod server;

pub use auth::{canonical, new_invite, read_auth, revoke, write_auth, AuthFile, Device, Invite};
#[cfg(feature = "serve")]
pub use serve::serve;
pub use server::{Bot, Op, Row, Server, Store};
