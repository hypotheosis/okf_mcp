//! A Redis for tests: the shared server `TEST_REDIS_URL` names, else a
//! `redis:7.4-alpine` container per [`TestRedis`] (testcontainers-modules'
//! default tag, 5.0, lacks GETDEL). [`TestRedis::start_cluster`] does the same
//! for a one-node Valkey cluster (`TEST_REDIS_CLUSTER_URL`, else a
//! `valkey/valkey:8.1-alpine` container). `scripts/test-redis.sh run` starts
//! both once for a whole run (issue #1045). Each [`TestRedis`] has its own
//! random key prefix, so tests sharing one server never see each other's keys.
//! A test that needs the server to itself (it stops it, changes the `default`
//! user, kills every pub/sub client) calls [`TestRedis::dedicated`], which
//! starts a container of its own when the server is shared.

// Each crate that includes this file uses a different part of it.
#![allow(dead_code)]

use std::time::Duration;

use testcontainers_modules::redis::{Redis as RedisImage, REDIS_PORT};
use testcontainers_modules::testcontainers::core::{ContainerPort, WaitFor};
use testcontainers_modules::testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, Image, ImageExt,
};

/// The variable naming a running Redis every test may share.
pub const SERVER_URL_VAR: &str = "TEST_REDIS_URL";
/// The variable naming a running one-node (or larger) Redis cluster.
pub const CLUSTER_URL_VAR: &str = "TEST_REDIS_CLUSTER_URL";
/// The variable a test's `[redis] url_env` names; [`TestRedis::env`] answers it.
pub const URL_VAR: &str = "OKF_TEST_REDIS_URL";

/// A key prefix no other [`TestRedis`] shares, even on a persistent
/// `TEST_REDIS_URL` server: `okftest:<pid>-<n>-<16 random hex digits>:`. The
/// pid and counter only help a person reading the keys; the random part is
/// what keeps runs in separate containers, or after a pid is reused, apart.
///
/// # Panics
/// If the operating system has no randomness: a broken test machine.
pub fn random_prefix() -> String {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("random bytes for the test key prefix");
    let random: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("okftest:{}-{random}:", super::pg::unique_start_label())
}

/// One test's Redis: its URL, its key prefix, and the container if it
/// started one (removed when this is dropped).
pub struct TestRedis {
    /// `redis://127.0.0.1:<port>/0`.
    pub url: String,
    /// [`random_prefix`], unique to this value.
    pub prefix: String,
    /// Whether the server is a cluster: the test's `[redis] mode` follows it.
    pub cluster: bool,
    container: Option<ContainerAsync<RedisImage>>,
    cluster_container: Option<ContainerAsync<GenericImage>>,
    /// The host port the container is pinned to (`None` on a shared server).
    port: Option<u16>,
}

/// How many times a start is tried when the port picked for it was taken
/// meanwhile (another test process picks from the same free range).
const PORT_ATTEMPTS: u32 = 5;

/// A host port nothing is listening on right now. The listener is dropped
/// before the container binds the port, so another process can still take it
/// first; [`start_pinned`] retries with a fresh port when that happens.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("0.0.0.0", 0))
        .and_then(|listener| listener.local_addr())
        .expect("a free host port")
        .port()
}

/// Starts the container `request(port)` builds with `port` pinned as the host
/// port, so a restart (`start_again`) keeps it. A start that fails (the port
/// was taken between choosing and binding it) is retried with a new port, up
/// to [`PORT_ATTEMPTS`] times (so any other start failure is retried too).
///
/// # Returns
/// The container and the port it is pinned to.
///
/// # Panics
/// If every attempt fails.
async fn start_pinned<I: Image>(
    request: impl Fn(u16) -> ContainerRequest<I>,
) -> (ContainerAsync<I>, u16) {
    let mut attempt = 1;
    loop {
        let port = free_port();
        let labelled = request(port).with_startup_timeout(super::pg::STARTUP_TIMEOUT);
        let image = labelled.image().name().to_string();
        let label = super::pg::unique_start_label();
        match super::pg::start_labelled(labelled, &label).await {
            Ok(container) => return (container, port),
            Err(error) if attempt < PORT_ATTEMPTS => {
                tracing::warn!("{image} container start on port {port} failed, retrying: {error}");
                attempt += 1;
            }
            Err(error) => panic!("start a {image} container (is docker reachable?): {error}"),
        }
    }
}

/// The shared server `var` names, if it is set to something.
fn shared_url(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|url| !url.is_empty())
}

impl TestRedis {
    /// A [`TestRedis`] on the shared server at `url` (no container of its own).
    fn on_shared_server(url: String, cluster: bool) -> Self {
        Self {
            url,
            prefix: random_prefix(),
            cluster,
            container: None,
            cluster_container: None,
            port: None,
        }
    }

    /// Uses `TEST_REDIS_URL`, or starts a labelled container on a host port
    /// chosen up front (so [`TestRedis::start_again`] keeps it), retried on a
    /// new port when a start fails.
    ///
    /// On the shared server a test must leave the server as it found it and
    /// touch only keys under [`TestRedis::prefix`]; one that cannot (it stops
    /// the server, changes the `default` user, kills every pub/sub client)
    /// calls [`TestRedis::dedicated`] first.
    ///
    /// # Panics
    /// If Docker cannot start the container.
    pub async fn start() -> Self {
        match shared_url(SERVER_URL_VAR) {
            Some(url) => Self::on_shared_server(url, false),
            None => Self::start_container().await,
        }
    }

    /// Uses `TEST_REDIS_CLUSTER_URL`, or starts a one-node Valkey cluster
    /// (see [`TestRedis::start`] for what a test may do on a shared one).
    ///
    /// # Panics
    /// If Docker cannot start the container or the cluster does not form
    /// within 30 s.
    pub async fn start_cluster() -> Self {
        match shared_url(CLUSTER_URL_VAR) {
            Some(url) => Self::on_shared_server(url, true),
            None => Self::start_cluster_container().await,
        }
    }

    /// This server, or, when it is a shared one, a container of the same kind
    /// started for this test alone, so the test may stop it, restart it,
    /// change its users or kill its connections without touching any other
    /// test. A [`TestRedis`] that already owns its container is returned as
    /// it is.
    ///
    /// # Panics
    /// As [`TestRedis::start`] or [`TestRedis::start_cluster`].
    pub async fn dedicated(self) -> Self {
        match (self.port, self.cluster) {
            (Some(_), _) => self,
            (None, false) => Self::start_container().await,
            (None, true) => Self::start_cluster_container().await,
        }
    }

    /// Starts a `redis:7.4-alpine` container on a pinned host port.
    async fn start_container() -> Self {
        let (container, port) = start_pinned(|port| {
            RedisImage::default()
                .with_tag("7.4-alpine")
                .with_mapped_port(port, ContainerPort::Tcp(REDIS_PORT))
        })
        .await;
        Self {
            url: format!("redis://127.0.0.1:{port}/0"),
            prefix: random_prefix(),
            cluster: false,
            container: Some(container),
            cluster_container: None,
            port: Some(port),
        }
    }

    /// Starts a one-node Valkey cluster: `--cluster-enabled yes`, then
    /// `cluster-announce-ip` and `cluster-announce-port` set to the pinned
    /// host port (so a cluster client on the host follows the topology back
    /// to it), all 16384 slots added, and a bounded wait for
    /// `cluster_state:ok`.
    async fn start_cluster_container() -> Self {
        let (container, port) = start_pinned(|port| {
            GenericImage::new("valkey/valkey", "8.1-alpine")
                .with_exposed_port(ContainerPort::Tcp(REDIS_PORT))
                .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
                .with_cmd([
                    "valkey-server",
                    "--cluster-enabled",
                    "yes",
                    "--cluster-config-file",
                    "nodes.conf",
                    "--appendonly",
                    "no",
                    "--save",
                    "",
                ])
                .with_mapped_port(port, ContainerPort::Tcp(REDIS_PORT))
        })
        .await;
        let url = format!("redis://127.0.0.1:{port}/0");
        tokio::time::timeout(Duration::from_secs(30), form_cluster(&url, port))
            .await
            .expect("the one-node cluster forms within 30 s");
        Self {
            url,
            prefix: random_prefix(),
            cluster: true,
            container: None,
            cluster_container: Some(container),
            port: Some(port),
        }
    }

    /// Creates a Redis user with the access string `rules` (an `ACL SETUSER`
    /// rule list) and returns the URL that signs in as it. The user's name
    /// and password are random, so tests sharing one server never meet.
    ///
    /// # Arguments
    /// * `rules` - for example the production string, `-@all +@read ...`.
    ///
    /// # Returns
    /// A `redis://<user>:<password>@host:port/<db>` URL for this server.
    ///
    /// # Panics
    /// If the server refuses `ACL SETUSER`, or the URL has no `redis://`.
    pub async fn url_for_user(&self, rules: &str) -> String {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).expect("random bytes for the test user");
        let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let password = format!("pw{name}");
        let user = format!("okftest{name}");
        let client = redis::Client::open(self.url.as_str()).expect("a redis URL");
        let mut connection = client
            .get_multiplexed_async_connection()
            .await
            .expect("connect as the default user");
        let mut command = redis::cmd("ACL");
        command
            .arg("SETUSER")
            .arg(&user)
            .arg("on")
            .arg(format!(">{password}"));
        for rule in rules.split_whitespace() {
            command.arg(rule);
        }
        let _: () = command
            .query_async(&mut connection)
            .await
            .expect("ACL SETUSER");
        let rest = self
            .url
            .strip_prefix("redis://")
            .expect("a redis:// test URL");
        format!("redis://{user}:{password}@{rest}")
    }

    /// An `env` reader for `Redis::connect` that knows only [`URL_VAR`].
    pub fn env(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |name| (name == URL_VAR).then(|| self.url.clone())
    }

    /// Stops the container (outage tests).
    ///
    /// # Panics
    /// On a shared server, which a test cannot stop for everyone (call
    /// [`TestRedis::dedicated`] first), or if Docker refuses the stop.
    pub async fn stop(&self) {
        match (&self.container, &self.cluster_container) {
            (Some(container), _) => container.stop().await,
            (None, Some(container)) => container.stop().await,
            (None, None) => panic!("a shared Redis cannot be stopped: use `dedicated()`"),
        }
        .expect("stop the redis container");
    }

    /// Starts a stopped container again on the same host port, so
    /// [`TestRedis::url`] and every client built from it reach the server
    /// again. A standalone server comes back empty (no persistence); a
    /// cluster node keeps its slots (`nodes.conf` survives in the container)
    /// and has its `cluster-announce-ip` and `-port` set again, which a
    /// restart loses. Returns once the cluster reports `cluster_state:ok`.
    ///
    /// # Panics
    /// If Docker refuses the start or the cluster does not recover within
    /// 30 s, or on a shared server (see [`TestRedis::stop`]).
    pub async fn start_again(&self) {
        match (&self.container, &self.cluster_container) {
            (Some(container), _) => container.start().await,
            (None, Some(container)) => container.start().await,
            (None, None) => panic!("a shared Redis cannot be restarted: use `dedicated()`"),
        }
        .expect("start the redis container");
        if self.cluster {
            let port = self.port.expect("a started cluster has a pinned port");
            tokio::time::timeout(Duration::from_secs(30), async {
                let mut connection = connect_when_ready(&self.url).await;
                announce(&mut connection, port).await;
                wait_for_cluster(&mut connection).await;
            })
            .await
            .expect("the one-node cluster recovers within 30 s");
        }
    }
}

/// Connects to the node at `url`.
///
/// # Panics
/// If the URL is invalid or the node does not answer.
async fn connect(url: &str) -> redis::aio::MultiplexedConnection {
    redis::Client::open(url)
        .expect("a valkey URL")
        .get_multiplexed_async_connection()
        .await
        .expect("connect to the valkey node")
}

/// Connects to the node at `url`, retrying while it refuses or resets the
/// connection (Docker's port forwarding can accept a connection a moment
/// before a restarted server does); the caller bounds the wait.
async fn connect_when_ready(url: &str) -> redis::aio::MultiplexedConnection {
    let client = redis::Client::open(url).expect("a valkey URL");
    loop {
        match client.get_multiplexed_async_connection().await {
            Ok(connection) => return connection,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Runs `CONFIG SET <name> <value>`.
///
/// # Panics
/// If the node refuses it.
async fn config_set(connection: &mut redis::aio::MultiplexedConnection, name: &str, value: &str) {
    let _: String = redis::cmd("CONFIG")
        .arg("SET")
        .arg(name)
        .arg(value)
        .query_async(connection)
        .await
        .expect("configure the one-node cluster");
}

/// Makes the node announce `127.0.0.1:<port>`, so a cluster client on the
/// host follows the topology back to the mapped port. `CONFIG SET` is not
/// persisted, so a restart needs this again.
async fn announce(connection: &mut redis::aio::MultiplexedConnection, port: u16) {
    config_set(connection, "cluster-announce-ip", "127.0.0.1").await;
    config_set(connection, "cluster-announce-port", &port.to_string()).await;
}

/// Returns when `CLUSTER INFO` says `cluster_state:ok` (the caller bounds
/// the wait).
///
/// # Panics
/// If `CLUSTER INFO` fails.
async fn wait_for_cluster(connection: &mut redis::aio::MultiplexedConnection) {
    loop {
        let info: String = redis::cmd("CLUSTER")
            .arg("INFO")
            .query_async(connection)
            .await
            .expect("read the cluster state");
        if info.contains("cluster_state:ok") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Turns the fresh cluster-enabled node at `url` into a one-node cluster
/// owning every slot, announcing `127.0.0.1:<port>`; returns when
/// `CLUSTER INFO` says `cluster_state:ok`.
///
/// # Panics
/// On any command failure (the caller bounds the wait).
async fn form_cluster(url: &str, port: u16) {
    let mut connection = connect(url).await;
    announce(&mut connection, port).await;
    let _: String = redis::cmd("CLUSTER")
        .arg("ADDSLOTSRANGE")
        .arg("0")
        .arg("16383")
        .query_async(&mut connection)
        .await
        .expect("add the cluster slots");
    wait_for_cluster(&mut connection).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_prefixes_have_the_documented_shape_and_never_repeat() {
        let first = random_prefix();
        let second = random_prefix();
        assert_ne!(first, second);
        for prefix in [&first, &second] {
            let body = prefix
                .strip_prefix("okftest:")
                .and_then(|rest| rest.strip_suffix(':'))
                .expect("okftest:...: shape");
            let random = body.rsplit('-').next().expect("a random part");
            assert_eq!(random.len(), 16, "{prefix}");
            assert!(random.bytes().all(|b| b.is_ascii_hexdigit()), "{prefix}");
        }
        // The random part differs even when the pid and counter could not.
        let tail = |prefix: &str| prefix.rsplit('-').next().map(str::to_string);
        assert_ne!(tail(&first), tail(&second));
    }
}
