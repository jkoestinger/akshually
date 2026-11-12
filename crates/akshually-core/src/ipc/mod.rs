//! The local channel between short-lived CLI calls and the one running app (ADR 0002): a Unix
//! socket in the runtime dir, guarded by a lock file so only one app instance serves it.

mod client;
pub mod proto;
mod server;

pub use client::{ClientError, Connection, connect};
pub use proto::{AppState, ErrorCode, ErrorReply, HumanCommand};
pub use server::{BindError, Server};
