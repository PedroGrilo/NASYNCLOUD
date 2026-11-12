//! nasyncloud: files on demand for WebDAV NAS boxes.
//!
//! * `remote` / `webdav`: talking to the server.
//! * `vfs` (Linux/macOS): the on-demand engine behind the FUSE mount.
//! * The Windows engine (Cloud Files API) lives in the binary crate.

pub mod activity;
pub mod config;
pub mod remote;
pub mod webdav;

#[cfg(unix)]
pub mod ranges;
#[cfg(unix)]
pub mod store;
#[cfg(unix)]
pub mod vfs;
