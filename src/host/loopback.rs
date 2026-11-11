//! Name resolution for the host's HTTP clients that tries a listening loopback
//! address first.
//!
//! `localhost` resolves to `::1` and `127.0.0.1`, and ureq tries them in order,
//! one at a time. A local server that listens on one family only (AnkiConnect,
//! local audio servers) makes the other attempt fail first, and Windows takes
//! about two seconds to report a refused loopback connection: every request
//! paid that. Only the order of the addresses changes; ureq still tries them all.

use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// How long a listening loopback address may take to accept the probe. An
/// accepting loopback socket answers in well under a millisecond.
const PROBE: Duration = Duration::from_millis(100);
/// How long the address found listening is reused before probing again.
const REMEMBER: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct LoopbackFirst {
    inner: DefaultResolver,
    /// The addresses last probed, the index that accepted, and when.
    remembered: Mutex<Vec<(Vec<SocketAddr>, usize, Instant)>>,
}

impl LoopbackFirst {
    fn listening(&self, addrs: &[SocketAddr]) -> Option<usize> {
        let now = Instant::now();
        let known = {
            let mut remembered = self.remembered.lock().unwrap_or_else(PoisonError::into_inner);
            remembered.retain(|(_, _, at)| now.duration_since(*at) < REMEMBER);
            remembered.iter().find(|(known, _, _)| known == addrs).map(|(_, index, _)| *index)
        };
        if known.is_some() {
            return known;
        }
        let index = probe(addrs)?;
        {
            let mut remembered = self.remembered.lock().unwrap_or_else(PoisonError::into_inner);
            remembered.retain(|(known, _, _)| known != addrs);
            remembered.push((addrs.to_vec(), index, now));
        }
        Some(index)
    }
}

/// The first address, in resolver order among those that answer in time, that
/// accepts a connection. Probes run concurrently; ones still pending are left
/// to finish on their own threads.
fn probe(addrs: &[SocketAddr]) -> Option<usize> {
    let (sender, receiver) = mpsc::channel();
    for (index, addr) in addrs.iter().copied().enumerate() {
        let sender = sender.clone();
        thread::spawn(move || {
            let _ = sender.send((index, TcpStream::connect_timeout(&addr, PROBE).is_ok()));
        });
    }
    drop(sender);
    let deadline = Instant::now() + PROBE;
    let mut accepted: Option<usize> = None;
    let mut answered = vec![false; addrs.len()];
    loop {
        // An earlier address that accepts is preferred, as ureq's order prefers it.
        if let Some(index) = accepted
            && answered[..index].iter().all(|done| *done)
        {
            return Some(index);
        }
        // Earlier addresses that have not answered by the deadline are not listening.
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { return accepted };
        match receiver.recv_timeout(left) {
            Ok((index, ok)) => {
                answered[index] = true;
                if ok && accepted.is_none_or(|current| index < current) {
                    accepted = Some(index);
                }
            }
            Err(_) => return accepted,
        }
    }
}

impl Resolver for LoopbackFirst {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addrs = self.inner.resolve(uri, config, timeout)?;
        if addrs.len() > 1
            && addrs.iter().all(|addr| addr.ip().is_loopback())
            && let Some(index) = self.listening(&addrs)
        {
            addrs[..=index].rotate_right(1);
        }
        Ok(addrs)
    }
}

/// An agent for `config` that resolves loopback names with [`LoopbackFirst`].
pub(crate) fn agent(config: Config) -> ureq::Agent {
    static RESOLVER: std::sync::OnceLock<std::sync::Arc<LoopbackFirst>> = std::sync::OnceLock::new();
    let shared = std::sync::Arc::clone(RESOLVER.get_or_init(Default::default));
    ureq::Agent::with_parts(config, DefaultConnector::default(), Shared(shared))
}

/// The process-wide resolver, so its remembered addresses outlive each agent.
#[derive(Debug)]
struct Shared(std::sync::Arc<LoopbackFirst>);

impl Resolver for Shared {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        self.0.resolve(uri, config, timeout)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn prefers_the_listening_address_and_keeps_the_others() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let closed: SocketAddr = format!("[::1]:{port}").parse().unwrap();
        let open: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // `::1` may be listening on the same port by coincidence only if
        // another process holds it; then either order is correct.
        if TcpStream::connect_timeout(&closed, PROBE).is_ok() {
            return;
        }
        assert_eq!(probe(&[closed, open]), Some(1));
        let resolver = LoopbackFirst::default();
        assert_eq!(resolver.listening(&[closed, open]), Some(1));
    }

    #[test]
    fn localhost_requests_skip_the_refused_family() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if TcpStream::connect_timeout(&format!("[::1]:{port}").parse().unwrap(), PROBE).is_ok() {
            return;
        }
        let server = thread::spawn(move || {
            // The probe's connection, then the request's.
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut buffer = [0; 1024];
                if stream.read(&mut buffer).unwrap_or(0) > 0 {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
                }
            }
        });
        let started = Instant::now();
        let mut response = agent(Config::default()).get(format!("http://localhost:{port}/")).call().unwrap();
        assert_eq!(response.body_mut().read_to_string().unwrap(), "ok");
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        server.join().unwrap();
    }

    #[test]
    fn keeps_order_when_nothing_listens() {
        // A freed ephemeral port can be taken by a parallel test: use port 1.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        if TcpStream::connect_timeout(&addr, PROBE).is_ok() {
            return;
        }
        let started = Instant::now();
        assert_eq!(probe(&[addr]), None);
        assert!(started.elapsed() < PROBE * 3);
    }

    #[test]
    fn earlier_listening_address_wins() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let second = TcpListener::bind("127.0.0.1:0").unwrap();
        let addrs = [first.local_addr().unwrap(), second.local_addr().unwrap()];
        assert_eq!(probe(&addrs), Some(0));
    }
}
