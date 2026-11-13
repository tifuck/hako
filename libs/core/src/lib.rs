//! Shared types, settings catalog, config, and IPC messages for hako.

pub mod catalog;
pub mod config;
pub mod device;
pub mod ipc;
pub mod mixer;
pub mod protocol;
pub mod remap;
pub mod socket;

pub use catalog::*;
pub use config::*;
pub use device::*;
pub use ipc::*;
pub use protocol::*;
pub use remap::*;
pub use socket::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SOCKET_NAME: &str = "hakod.sock";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Resolve the daemon socket path.
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return std::path::PathBuf::from(runtime).join(SOCKET_NAME);
        }
    }
    // logind's per-user runtime dir; never a shared dir like /tmp.
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    std::path::PathBuf::from(format!("/run/user/{uid}")).join(SOCKET_NAME)
}
