//! A TCP proxy that can be broken on purpose.
//!
//! `IMPLEMENTATION.md` §4 lists "kill the API server mid-watch" as a test, not
//! an edge case. Actually killing it — `docker stop` on the `kind` node — takes
//! the cluster down for every other test in the run and comes back slowly and
//! unpredictably. Interposing a proxy and cutting it produces the same thing at
//! the socket the app is actually holding: established connections are reset,
//! new ones are refused, and nothing else in the cluster notices.
//!
//! Bytes are copied verbatim, so TLS passes through untouched. Pointing a
//! kubeconfig at `https://127.0.0.1:<proxy port>` still verifies against the
//! cluster's own CA, because `kind`'s serving certificate carries `127.0.0.1`
//! and certificates say nothing about ports.

use std::io::Read as _;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the accept loop sleeps between polls when nothing is arriving.
const IDLE: Duration = Duration::from_millis(10);

/// A proxy in front of an upstream address, with a switch on it.
#[derive(Debug)]
pub struct Proxy {
    port: u16,
    upstream: SocketAddr,
    /// Whether connections are being carried at all.
    open: Arc<AtomicBool>,
    /// Set on drop, to end the accept loop.
    stopped: Arc<AtomicBool>,
    /// Every socket currently in flight, so they can all be cut at once.
    live: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    /// Starts a proxy on a free loopback port, forwarding to `upstream`.
    pub fn to(upstream: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;

        let proxy = Self {
            port,
            upstream,
            open: Arc::new(AtomicBool::new(true)),
            stopped: Arc::new(AtomicBool::new(false)),
            live: Arc::new(Mutex::new(Vec::new())),
        };

        let (open, stopped, live) = (
            Arc::clone(&proxy.open),
            Arc::clone(&proxy.stopped),
            Arc::clone(&proxy.live),
        );
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        if !open.load(Ordering::SeqCst) {
                            // Refused while the fault is in place, which is what
                            // an apiserver that is not there does.
                            let _ = client.shutdown(Shutdown::Both);
                            continue;
                        }
                        let _ = client.set_nonblocking(false);
                        carry(client, upstream, &live);
                    }
                    // Every accept error is transient here, and treating any of
                    // them as fatal is a trap: cutting the proxy makes clients
                    // reconnect and give up mid-handshake, which surfaces as
                    // ECONNABORTED on the *next* accept. Returning there would
                    // kill the proxy for good and make `restore` a lie.
                    Err(_) => std::thread::sleep(IDLE),
                }
            }
        });

        Ok(proxy)
    }

    /// A proxy in front of whatever the test context's kubeconfig points at.
    pub fn to_test_cluster() -> std::io::Result<Self> {
        Self::to(upstream_of(&crate::exec::test_cluster()))
    }

    /// The local port to point a client at.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Where it forwards to.
    pub fn upstream(&self) -> SocketAddr {
        self.upstream
    }

    /// Cuts every connection and refuses new ones.
    ///
    /// Both halves of every live socket are shut down, so a client blocked on a
    /// watch stream sees the connection reset rather than a stall — the same
    /// thing it would see if the apiserver went away.
    pub fn interrupt(&self) {
        self.open.store(false, Ordering::SeqCst);
        for socket in self
            .live
            .lock()
            .expect("the socket list is not poisoned")
            .drain(..)
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    /// Starts carrying connections again.
    pub fn restore(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.interrupt();
    }
}

/// Connects to the upstream and pumps one client connection both ways.
fn carry(client: TcpStream, upstream: SocketAddr, live: &Arc<Mutex<Vec<TcpStream>>>) {
    let Ok(server) = TcpStream::connect_timeout(&upstream, Duration::from_secs(10)) else {
        let _ = client.shutdown(Shutdown::Both);
        return;
    };

    // Keep a handle on both ends so `interrupt` can cut them mid-stream.
    let mut sockets = live.lock().expect("the socket list is not poisoned");
    for socket in [&client, &server] {
        if let Ok(handle) = socket.try_clone() {
            sockets.push(handle);
        }
    }
    drop(sockets);

    pump(client.try_clone(), server.try_clone());
    pump(server.try_clone(), client.try_clone());
}

/// Copies one direction on its own thread, until either end hangs up.
fn pump(from: std::io::Result<TcpStream>, to: std::io::Result<TcpStream>) {
    let (Ok(mut from), Ok(mut to)) = (from, to) else {
        return;
    };

    std::thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match from.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if std::io::Write::write_all(&mut to, &buffer[..read]).is_err() {
                        break;
                    }
                }
            }
        }
        // One direction closing closes the connection: a half-open socket to a
        // dead apiserver is exactly the stall this exists to avoid.
        let _ = from.shutdown(Shutdown::Both);
        let _ = to.shutdown(Shutdown::Both);
    });
}

/// The host and port a kubeconfig cluster entry points at.
pub fn upstream_of(cluster: &serde_json::Value) -> SocketAddr {
    let server = cluster["cluster"]["server"]
        .as_str()
        .expect("the cluster entry names a server");
    let rest = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("http://"))
        .unwrap_or(server);

    let (host, port) = rest
        .trim_end_matches('/')
        .rsplit_once(':')
        .unwrap_or((rest, "443"));

    // `kind` publishes on 127.0.0.1; resolving keeps this honest if it ever
    // publishes on a name instead.
    use std::net::ToSocketAddrs as _;
    format!("{host}:{port}")
        .to_socket_addrs()
        .unwrap_or_else(|error| panic!("could not resolve {host}:{port}: {error}"))
        .next()
        .unwrap_or_else(|| panic!("{host}:{port} resolved to nothing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A trivial echo server, so the proxy can be tested without a cluster.
    fn echo() -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        let address = listener.local_addr().expect("has an address");

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut buffer = [0_u8; 1024];
                    while let Ok(read) = stream.read(&mut buffer) {
                        if read == 0 || stream.write_all(&buffer[..read]).is_err() {
                            return;
                        }
                    }
                });
            }
        });

        address
    }

    fn round_trip(port: u16) -> std::io::Result<String> {
        let mut socket = TcpStream::connect(("127.0.0.1", port))?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.write_all(b"hello")?;

        let mut buffer = [0_u8; 5];
        std::io::Read::read_exact(&mut socket, &mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).to_string())
    }

    #[test]
    fn a_proxy_carries_traffic_until_it_is_interrupted() {
        let proxy = Proxy::to(echo()).expect("the proxy starts");
        assert_eq!(round_trip(proxy.port()).expect("carries"), "hello");

        proxy.interrupt();
        assert!(
            round_trip(proxy.port()).is_err(),
            "an interrupted proxy must not carry traffic"
        );

        proxy.restore();
        assert_eq!(
            round_trip(proxy.port()).expect("carries again"),
            "hello",
            "restoring must bring it back"
        );
    }

    #[test]
    fn an_established_connection_is_cut_not_left_hanging() {
        // The point of the whole module: a client already holding a connection —
        // a watch stream — must see it break, not stall forever.
        let proxy = Proxy::to(echo()).expect("the proxy starts");
        let mut socket =
            TcpStream::connect(("127.0.0.1", proxy.port())).expect("the proxy accepts");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout is set");
        socket.write_all(b"hello").expect("writes");
        let mut buffer = [0_u8; 5];
        std::io::Read::read_exact(&mut socket, &mut buffer).expect("echoes");

        proxy.interrupt();

        // Reading now ends — with EOF or an error — rather than blocking until
        // the read timeout.
        let mut rest = Vec::new();
        let read = socket.read_to_end(&mut rest);
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the connection should be gone, got {read:?}"
        );
    }

    #[test]
    fn a_server_url_is_split_into_something_connectable() {
        let cluster = serde_json::json!({
            "cluster": { "server": "https://127.0.0.1:56196" }
        });
        let address = upstream_of(&cluster);

        assert_eq!(address.port(), 56_196);
        assert!(address.ip().is_loopback());
    }
}
