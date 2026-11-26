//! Test helper for an address that refuses connections, without a free port.
//!
//! A test that needs "nothing answers here" used to bind `127.0.0.1:0`, read
//! the port and drop the listener. Between the drop and the connect another
//! test in the same process (or another process on the machine) can bind that
//! ephemeral port, and the test then gets an answer, or waits on a listener
//! that never replies (issue #769, the same race as the NTP test of #707).
//!
//! [`Refused`] keeps the port instead: a socket that is bound but never put
//! into the listening state. On Linux a TCP connect to such a port is refused
//! at once (`ECONNREFUSED`), because only listening sockets take connections,
//! and while the socket is held the kernel gives the port to nobody else.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpSocket;

/// The longest any bounded step of a test may take (lessons.md: every wait
/// has a timeout).
pub(crate) const STEP: Duration = Duration::from_secs(30);

/// A loopback address whose port is held but not listening, so a connect to
/// it is refused. Keep the value alive for as long as the test connects.
pub(crate) struct Refused {
    /// Held so the port stays ours; never listened on.
    _socket: TcpSocket,
    address: SocketAddr,
}

impl Refused {
    /// Binds a loopback port and keeps it without listening.
    ///
    /// # Panics
    /// If the loopback socket cannot be created or bound.
    pub(crate) fn new() -> Self {
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
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    /// The refusing port.
    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }
}

/// Runs `step` for at most [`STEP`]; a step that hangs (for example because a
/// refusing address turned out to answer) fails fast naming what it waited
/// for.
///
/// # Panics
/// If `step` does not finish within [`STEP`].
pub(crate) async fn bounded<T>(what: &str, step: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, step)
        .await
        .unwrap_or_else(|_| panic!("timed out after {STEP:?} waiting for {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[tokio::test]
    async fn a_held_unlistened_port_refuses_every_connect() {
        let refused = Refused::new();

        for _ in 0..3 {
            let error = bounded(
                "a connect to the refusing address",
                tokio::net::TcpStream::connect(refused.address()),
            )
            .await
            .expect_err("nothing listens there");
            assert_eq!(error.kind(), ErrorKind::ConnectionRefused, "{error:?}");
        }
    }

    #[tokio::test]
    async fn the_held_port_is_not_given_to_another_bind() {
        let refused = Refused::new();

        let second = std::net::TcpListener::bind(refused.address());

        assert!(second.is_err(), "the port was handed out twice");
    }

    #[tokio::test]
    #[should_panic(expected = "timed out after")]
    async fn a_step_that_never_finishes_fails_with_its_name() {
        tokio::time::pause();
        bounded("a step that hangs", std::future::pending::<()>()).await;
    }
}
