//! Local daemon IPC: KernelClient over a Unix socket.
//!
//! Compiled only on Unix (`#[cfg(unix)]` at the crate root). The
//! `#[cfg(unix)]` attributes that remain inside are therefore always true;
//! they predate the gate and are left so the diff that introduced it stayed
//! reviewable. `ListenSpec::NamedPipe` is accepted by the spec parser and
//! refused at dial as `UnsupportedEndpoint` — there is no Windows transport.

pub mod client;
pub mod server;

pub use client::{
    DaemonClient, DaemonClientError, DaemonEventStream, IpcRewindResult, IpcTurnHandle,
    MAX_CONNECT_WAIT, MAX_RECONNECT_ATTEMPTS, RequestIdempotency,
};
pub use server::{
    IPC_SCHEMA, IpcError, IpcLimits, IpcServer, IpcServerGuard, ListenSpec, MAX_CONNECTIONS,
    MAX_FRAME_BYTES, MAX_REQUEST_ID_BYTES, SOCKET_DIR_MODE, SOCKET_FILE_MODE, read_frame,
    write_frame,
};
