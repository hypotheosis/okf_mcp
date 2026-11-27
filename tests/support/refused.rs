//! An address that refuses connections, for integration tests (issue #1095).
//!
//! The same approach as the crate-private `src/test_net.rs`, which tests under
//! `tests/` cannot import: [`Refused`] holds a loopback port that is bound but
//! never listened on, so every connect to it is refused at once and no other
//! bind can take the port while the value lives. Never use a fixed "dead" port
//! such as `127.0.0.1:9`, and never bind `:0` and drop the listener.

// Each crate that includes this file uses a different part of it.
#![allow(dead_code)]

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpSocket;

/// The longest any bounded step of a test may take.
pub const STEP: Duration = Duration::from_secs(30);

/// A loopback address whose port is held but not listening. Keep the value
/// alive for as long as the test connects.
pub struct Refused {
    /// Held so the port stays ours; never listened on.
    _socket: TcpSocket,
    address: SocketAddr,
}

impl Refused {
    /// Binds a loopback port and keeps it without listening.
    ///
    /// # Panics
    /// If the loopback socket cannot be created or bound.
    pub fn new() -> Self {
        let socket = TcpSocket::new_v4().expect("create a TCP socket");
        socket
            .bind("127.0.0.1:0".parse().expect("a loopback address"))
            .expect("bind a loopback port");
        let address = socket.local_addr().expect("the bound address");
        Self {
            _socket: socket,
            address,
        }
    }

    /// The refusing address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

/// Runs `step` for at most [`STEP`], failing with `what` if it hangs.
///
/// # Panics
/// If `step` does not finish within [`STEP`].
pub async fn bounded<T>(what: &str, step: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, step)
        .await
        .unwrap_or_else(|_| panic!("timed out after {STEP:?} waiting for {what}"))
}
