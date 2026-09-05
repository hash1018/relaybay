//! The standalone relay.
//!
//! Everything it does lives in the library, so an application that already
//! has a media pipeline can embed the same server rather than run this. What
//! is here is a configuration and a wait.

use std::io;

use relaybay::server::{Config, Server};

fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let server = Server::start(Config::default())?;
    tracing::info!("relaybay {}", env!("CARGO_PKG_VERSION"));

    // Nothing to do on this thread but stay alive: the work is on the
    // runtime the handle owns, and dropping the handle would stop it.
    wait_for_a_reason_to_stop();

    tracing::info!("shutting down");
    server.shutdown();
    Ok(())
}

/// Blocks until the process is asked to stop.
///
/// Reading standard input to end of file rather than installing a signal
/// handler: it ends on Ctrl-D, on a closed pipe, and on the terminal going
/// away, which covers running this by hand and running it under a
/// supervisor. A signal handler comes with the configuration work, when
/// there is some.
fn wait_for_a_reason_to_stop() {
    let mut line = String::new();
    while io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
        line.clear();
    }
}
