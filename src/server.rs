//! Starting and stopping the whole thing, without asking the caller to be
//! asynchronous.
//!
//! The layers under this use a runtime — sockets have to wait on something,
//! and a publisher displaced by another has to be woken while its own socket
//! is silent. None of that reaches a caller. [`Server::start`] builds a
//! runtime, keeps it inside the handle it returns, and hands back an
//! ordinary value that an application on ordinary threads can hold, ask
//! questions of, and drop.
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! let server = relaybay::server::Server::start(Default::default())?;
//! // … the application's own threads run as they always did …
//! server.shutdown();
//! # Ok(())
//! # }
//! ```
//!
//! An application that already has a runtime should use
//! [`Server::start_on`] instead, so that its process does not end up with
//! two sets of worker threads competing for the same cores.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

use crate::path::Registry;
use crate::rtmp;
use crate::rtsp;

/// What to serve, and with what.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where to accept RTMP publishers, or `None` not to.
    pub rtmp: Option<SocketAddr>,

    /// Where to accept RTSP players, or `None` not to.
    pub rtsp: Option<SocketAddr>,

    /// How many threads the runtime [`Server::start`] builds will use.
    ///
    /// Two rather than one so that a long copy on one connection does not
    /// hold up every other, and two rather than one per core because this is
    /// meant to sit inside an application that has its own work to do. A
    /// relay's threads spend their time waiting on sockets.
    ///
    /// Ignored by [`Server::start_on`], which uses the runtime it is given.
    pub worker_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rtmp: Some(([0, 0, 0, 0], 1935).into()),
            rtsp: Some(([0, 0, 0, 0], 8554).into()),
            worker_threads: 2,
        }
    }
}

/// Starts a server. See the module docs.
pub struct Server;

impl Server {
    /// Builds a runtime, starts everything on it, and hands back a handle
    /// that owns both.
    ///
    /// Binding happens before this returns, so a port already in use is an
    /// error here rather than a silence later.
    pub fn start(config: Config) -> io::Result<ServerHandle> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads.max(1))
            .thread_name("relaybay")
            // Timers as well as sockets: a write that never completes is
            // given a deadline rather than waited on forever, and a runtime
            // without them panics at the first one rather than at startup.
            .enable_all()
            .build()?;
        let mut handle = Self::start_on(config, runtime.handle().clone())?;
        handle.runtime = Some(runtime);
        Ok(handle)
    }

    /// Starts everything on a runtime someone else owns.
    ///
    /// Safe to call from inside that runtime: the listeners are bound with
    /// the standard library and handed over, so nothing here blocks on a
    /// future.
    ///
    /// The runtime needs both I/O and timers — a `Builder::enable_all`, or
    /// `enable_io` and `enable_time` together. A write to a reader that has
    /// stopped taking bytes is given a deadline rather than waited on
    /// forever, and a runtime without timers panics at the first of those
    /// rather than here.
    pub fn start_on(config: Config, runtime: Handle) -> io::Result<ServerHandle> {
        let registry = Registry::new();
        let mut tasks = Vec::new();

        if let Some(address) = config.rtmp {
            let listener = bind(address, &runtime)?;
            tracing::info!(%address, "accepting RTMP publishers");
            tasks.push(runtime.spawn(rtmp::server::serve(listener, Arc::clone(&registry))));
        }
        if let Some(address) = config.rtsp {
            let listener = bind(address, &runtime)?;
            tracing::info!(%address, "accepting RTSP players");
            tasks.push(runtime.spawn(rtsp::server::serve(listener, Arc::clone(&registry))));
        }

        Ok(ServerHandle {
            registry,
            tasks,
            runtime: None,
        })
    }
}

/// A running server.
///
/// Dropping it stops everything. Each listener owns the connections it
/// accepted, so ending the listener ends them too.
pub struct ServerHandle {
    registry: Arc<Registry>,
    tasks: Vec<JoinHandle<()>>,
    /// Present only when [`Server::start`] built it. A handle from
    /// [`Server::start_on`] leaves the runtime to whoever made it.
    runtime: Option<Runtime>,
}

impl ServerHandle {
    /// The paths being published, and the way to read them.
    ///
    /// What an application embedding this reaches for: it can list what is
    /// live, and attach a reader of its own without going back out over a
    /// socket.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Stops accepting, ends every connection, and waits for the runtime
    /// this owns to wind down.
    ///
    /// Call this from an ordinary thread. Dropping the handle does the same
    /// thing without waiting.
    pub fn shutdown(mut self) {
        self.stop();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        }
    }

    fn stop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("ServerHandle")
            .field("paths", &self.registry.names())
            .field("listeners", &self.tasks.len())
            .field("owns_runtime", &self.runtime.is_some())
            .finish()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
        // `shutdown_background` rather than a plain drop: dropping a runtime
        // waits for its threads, and waiting is not allowed on a thread that
        // is itself running one. An application that dropped this from
        // inside its own runtime would panic instead of shutting down.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Binds with the standard library and hands the socket to the runtime.
///
/// Rather than `TcpListener::bind`, which is a future and would have to be
/// waited on — something a caller already inside a runtime cannot do. This
/// way binding is synchronous everywhere and its errors arrive at the call
/// that caused them.
fn bind(address: SocketAddr, runtime: &Handle) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    // `from_std` registers the socket with the runtime's reactor, which
    // means there has to be one to register with.
    let _guard = runtime.enter();
    TcpListener::from_std(listener)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> Config {
        Config {
            rtmp: Some(([127, 0, 0, 1], 0).into()),
            rtsp: Some(([127, 0, 0, 1], 0).into()),
            worker_threads: 1,
        }
    }

    #[test]
    fn a_server_starts_and_stops_from_an_ordinary_thread() {
        // No runtime here, and none reaches this code. That is the whole
        // point of the type.
        let server = Server::start(config()).expect("bound");
        assert!(server.registry().names().is_empty());
        server.shutdown();
    }

    #[test]
    fn a_port_already_taken_is_an_error_where_it_happened() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = taken.local_addr().unwrap();
        let error = Server::start(Config {
            rtmp: Some(address),
            rtsp: None,
            worker_threads: 1,
        })
        .expect_err("the port is in use");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn a_server_that_serves_nothing_starts() {
        let server = Server::start(Config {
            rtmp: None,
            rtsp: None,
            worker_threads: 1,
        })
        .unwrap();
        server.shutdown();
    }

    #[tokio::test]
    async fn a_runtime_that_already_exists_is_used_rather_than_a_second_one() {
        // `start` here would build a runtime inside a runtime and then have
        // to be dropped from one, which is what `start_on` exists to avoid.
        let server = Server::start_on(config(), Handle::current()).expect("bound");
        assert!(server.registry().names().is_empty());
        drop(server);
        // The task is aborted rather than left listening.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
