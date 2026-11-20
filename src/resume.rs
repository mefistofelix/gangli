//! Resumable streams (section 6 of docs/ARCHITECTURE.md): sessions and tunnels that survive a dropped
//! connection. The application (an SSH channel, a local TCP connection, a process on the node) talks to
//! one half of an in-memory duplex pipe that never breaks; a *pump* task moves the bytes between the other
//! half and the current QUIC stream.
//!
//! Both pumps keep the bytes the other side has not acknowledged. When the QUIC stream dies, the console
//! side dials the machine again and sends `Resume { id, received }`; the node answers with what it received;
//! each side retransmits from the other's offset and goes on. Nothing is lost or duplicated. If the
//! connection does not come back within the grace period, the stream ends like a normal close.
//!
//! Frames on the QUIC stream: `DATA` (bytes, offsets implicit), `ACK` (u64 bytes received), `FIN` (u64
//! total bytes sent: no more data).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

use crate::proto::sframe;

const DATA: u8 = 1;
const ACK: u8 = 2;
const FIN: u8 = 3;
/// Unacknowledged bytes kept per direction; the application is slowed down beyond this.
const MAX_UNACKED: usize = 8 << 20;
const CHUNK: usize = 32 * 1024;
const ACK_EVERY: u64 = 64 * 1024;

pub type Writer = Box<dyn AsyncWrite + Unpin + Send>;
pub type Reader = Box<dyn AsyncRead + Unpin + Send>;

/// A QUIC stream to (re)attach, with the number of bytes the peer says it has received from us.
pub struct Transport {
    pub send: Writer,
    pub recv: Reader,
    pub peer_received: u64,
}

/// Dial again and reattach; `Ok(None)`: the other side no longer has the stream (stop trying).
pub type Reconnect = Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = Result<Option<Transport>>> + Send>> + Send + Sync>;

/// Sent to a pump to make it give up at once.
const GONE: u64 = u64::MAX;

/// Handle kept by the node's registry: where to send a new transport, and how much has been received.
#[derive(Clone)]
pub struct Attach {
    pub tx: mpsc::UnboundedSender<Transport>,
    pub received: Arc<AtomicU64>,
}

enum Event {
    Frame(u8, Vec<u8>),
    Lost(u64),
}

/// Start a pump. Returns the application's half and the attach handle. `reconnect` is set on the dialing
/// side (the console): it is called with our received offset each time the stream is lost.
pub fn start(first: Transport, grace: Duration, reconnect: Option<Reconnect>) -> (DuplexStream, Attach, tokio::task::JoinHandle<()>) {
    let (app, inner) = tokio::io::duplex(256 * 1024);
    let (tx, rx) = mpsc::unbounded_channel();
    let received = Arc::new(AtomicU64::new(0));
    let attach = Attach { tx: tx.clone(), received: received.clone() };
    let task = tokio::spawn(pump(inner, first, rx, tx, received, grace, reconnect));
    (app, attach, task)
}

struct Link {
    send: Writer,
    generation: u64,
}

#[allow(clippy::too_many_arguments)]
async fn pump(
    inner: DuplexStream,
    first: Transport,
    mut reattach: mpsc::UnboundedReceiver<Transport>,
    reattach_tx: mpsc::UnboundedSender<Transport>,
    received: Arc<AtomicU64>,
    grace: Duration,
    reconnect: Option<Reconnect>,
) {
    let (mut app_r, mut app_w) = tokio::io::split(inner);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    let mut unacked: VecDeque<u8> = VecDeque::new();
    let mut base: u64 = 0; // offset of unacked[0]
    let mut rx_total: u64 = 0;
    let mut acked_sent: u64 = 0;
    let mut app_eof = false;
    let mut fin_sent = false;
    let mut peer_fin: Option<u64> = None;
    let mut app_w_closed = false;
    let mut generation = 0u64;
    let mut lost_since: Option<Instant> = None;
    let mut reconnecting = false;
    let mut buf = vec![0u8; CHUNK];

    let attach_link = |t: Transport, generation: u64, ev_tx: &mpsc::UnboundedSender<Event>| -> (Link, u64) {
        let mut recv = t.recv;
        let ev = ev_tx.clone();
        tokio::spawn(async move {
            loop {
                match sframe::read(&mut recv).await {
                    Ok((k, b)) => {
                        if ev.send(Event::Frame(k, b)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = ev.send(Event::Lost(generation));
                        break;
                    }
                }
            }
        });
        (Link { send: t.send, generation }, t.peer_received)
    };

    let (l, peer) = attach_link(first, generation, &ev_tx);
    let mut link = Some(l);
    if peer != 0 {
        // A fresh stream starts at zero on both sides.
        return;
    }

    loop {
        // Done: everything sent and acknowledged, and the peer's end delivered.
        if fin_sent && unacked.is_empty() && peer_fin.is_some_and(|f| rx_total >= f) {
            if let Some(l) = link.as_mut() {
                let _ = sframe::write(&mut l.send, ACK, &rx_total.to_be_bytes()).await;
                let _ = l.send.shutdown().await;
            }
            break;
        }
        if lost_since.is_some_and(|t| t.elapsed() > grace) {
            break;
        }
        let can_read_app = !app_eof && unacked.len() < MAX_UNACKED;
        tokio::select! {
            r = app_r.read(&mut buf), if can_read_app => {
                match r {
                    Ok(0) | Err(_) => {
                        app_eof = true;
                        if let Some(l) = link.as_mut() {
                            if sframe::write(&mut l.send, FIN, &(base + unacked.len() as u64).to_be_bytes()).await.is_ok() {
                                fin_sent = true;
                            }
                        }
                    }
                    Ok(n) => {
                        unacked.extend(&buf[..n]);
                        if let Some(l) = link.as_mut() {
                            let _ = sframe::write(&mut l.send, DATA, &buf[..n]).await;
                        }
                    }
                }
            }
            ev = ev_rx.recv() => {
                match ev {
                    Some(Event::Frame(DATA, d)) => {
                        rx_total += d.len() as u64;
                        received.store(rx_total, Ordering::SeqCst);
                        if !app_w_closed && app_w.write_all(&d).await.is_err() {
                            app_w_closed = true;
                        }
                        if rx_total - acked_sent >= ACK_EVERY {
                            if let Some(l) = link.as_mut() {
                                let _ = sframe::write(&mut l.send, ACK, &rx_total.to_be_bytes()).await;
                            }
                            acked_sent = rx_total;
                        }
                    }
                    Some(Event::Frame(ACK, b)) if b.len() == 8 => {
                        let n = u64::from_be_bytes(b.try_into().unwrap());
                        if n > base {
                            let drop = ((n - base) as usize).min(unacked.len());
                            unacked.drain(..drop);
                            base += drop as u64;
                        }
                    }
                    Some(Event::Frame(FIN, b)) if b.len() == 8 => {
                        let f = u64::from_be_bytes(b.try_into().unwrap());
                        peer_fin = Some(f);
                        if rx_total >= f && !app_w_closed {
                            let _ = app_w.shutdown().await;
                            app_w_closed = true;
                        }
                        if let Some(l) = link.as_mut() {
                            let _ = sframe::write(&mut l.send, ACK, &rx_total.to_be_bytes()).await;
                        }
                        acked_sent = rx_total;
                    }
                    Some(Event::Frame(_, _)) => {}
                    Some(Event::Lost(g)) => {
                        if link.as_ref().is_some_and(|l| l.generation == g) {
                            link = None;
                            lost_since.get_or_insert_with(Instant::now);
                            if let (Some(rc), false) = (&reconnect, reconnecting) {
                                reconnecting = true;
                                let (rc, tx, rcv) = (rc.clone(), reattach_tx.clone(), received.clone());
                                let deadline = Instant::now() + grace;
                                tokio::spawn(async move {
                                    let mut wait = Duration::from_millis(500);
                                    while Instant::now() < deadline {
                                        match rc(rcv.load(Ordering::SeqCst)).await {
                                            Ok(Some(t)) => {
                                                let _ = tx.send(t);
                                                return;
                                            }
                                            Ok(None) => {
                                                let (a, _) = tokio::io::duplex(1);
                                                let (r, w) = tokio::io::split(a);
                                                let _ = tx.send(Transport { send: Box::new(w), recv: Box::new(r), peer_received: GONE });
                                                return;
                                            }
                                            Err(_) => {}
                                        }
                                        tokio::time::sleep(wait).await;
                                        wait = (wait * 2).min(Duration::from_secs(10));
                                    }
                                });
                            }
                        }
                    }
                    None => break,
                }
            }
            t = reattach.recv() => {
                let Some(t) = t else { break };
                if t.peer_received == GONE {
                    break;
                }
                reconnecting = false;
                generation += 1;
                let (mut l, peer) = attach_link(t, generation, &ev_tx);
                // Forget what the peer already has, resend the rest.
                if peer < base {
                    break; // data the peer never got is gone: cannot resume
                }
                let drop = ((peer - base) as usize).min(unacked.len());
                unacked.drain(..drop);
                base += drop as u64;
                let pending: Vec<u8> = unacked.iter().copied().collect();
                let mut ok = true;
                for c in pending.chunks(CHUNK) {
                    if sframe::write(&mut l.send, DATA, c).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                if ok && app_eof {
                    ok = sframe::write(&mut l.send, FIN, &(base + unacked.len() as u64).to_be_bytes()).await.is_ok();
                    fin_sent = ok;
                }
                if ok {
                    let _ = sframe::write(&mut l.send, ACK, &rx_total.to_be_bytes()).await;
                    acked_sent = rx_total;
                    link = Some(l);
                    lost_since = None;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)), if rx_total > acked_sent && link.is_some() => {
                if let Some(l) = link.as_mut() {
                    let _ = sframe::write(&mut l.send, ACK, &rx_total.to_be_bytes()).await;
                }
                acked_sent = rx_total;
            }
        }
    }
    if !app_w_closed {
        let _ = app_w.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two pumps over in-memory "QUIC streams"; the link is cut in the middle and reattached.
    #[tokio::test(flavor = "multi_thread")]
    async fn survives_a_cut() {
        let wire = || {
            let (a, b) = tokio::io::duplex(1 << 16);
            let (ar, aw) = tokio::io::split(a);
            let (br, bw) = tokio::io::split(b);
            (Transport { send: Box::new(aw), recv: Box::new(ar), peer_received: 0 }, Transport { send: Box::new(bw), recv: Box::new(br), peer_received: 0 })
        };
        let (t1, t2) = wire();
        let (mut left, left_attach, _) = start(t1, Duration::from_secs(10), None);
        let (mut right, right_attach, _) = start(t2, Duration::from_secs(10), None);

        let first: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        left.write_all(&first).await.unwrap();
        let mut got = vec![0u8; first.len()];
        right.read_exact(&mut got).await.unwrap();
        assert_eq!(got, first);

        // Cut: give both pumps a new wire, each told what the other received.
        let (mut n1, mut n2) = wire();
        n1.peer_received = right_attach.received.load(Ordering::SeqCst);
        n2.peer_received = left_attach.received.load(Ordering::SeqCst);
        left_attach.tx.send(n1).unwrap();
        right_attach.tx.send(n2).unwrap();

        let second: Vec<u8> = (0..50_000u32).map(|i| (i % 13) as u8).collect();
        left.write_all(&second).await.unwrap();
        right.write_all(b"back").await.unwrap();
        let mut got = vec![0u8; second.len()];
        right.read_exact(&mut got).await.unwrap();
        assert_eq!(got, second);
        let mut back = [0u8; 4];
        left.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"back");
    }
}
