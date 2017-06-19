use super::super::connection::{Connection as _Connection, ctx};
use super::super::connector;
use futures::{self, Future, Poll};
use std::{io, net};
use std::cell::{Ref, RefCell};
use std::rc::Rc;
use std::time::Instant;
use tacho;

pub type Connection = _Connection<Ctx>;

pub fn new(peer_addr: net::SocketAddr, weight: f64) -> Endpoint {
    let state = Rc::new(RefCell::new(State::default()));
    Endpoint {
        peer_addr,
        weight,
        state,
    }
}

#[derive(Default)]
pub struct State {
    pub pending_conns: usize,
    pub open_conns: usize,
    pub consecutive_failures: usize,
    pub rx_bytes: usize,
    pub tx_bytes: usize,
}

impl State {
    pub fn load(&self) -> usize {
        self.open_conns + self.pending_conns
    }
    pub fn is_idle(&self) -> bool {
        self.open_conns == 0
    }
}

/// Represents a single concrete traffic destination
pub struct Endpoint {
    peer_addr: net::SocketAddr,
    weight: f64,
    state: Rc<RefCell<State>>,
}

impl Endpoint {
    pub fn peer_addr(&self) -> net::SocketAddr {
        self.peer_addr
    }

    pub fn state(&self) -> Ref<State> {
        self.state.borrow()
    }

    // TODO we should be able to use throughput/bandwidth as well.
    pub fn load(&self) -> usize {
        self.state.borrow().load()
    }

    pub fn set_weight(&mut self, w: f64) {
        assert!(0.0 <= w && w <= 1.0);
        self.weight = w;
    }

    pub fn weight(&self) -> f64 {
        self.weight
    }

    pub fn connect(&self, sock: connector::Connecting, duration: &tacho::Timer) -> Connecting {
        let conn = {
            let peer_addr = self.peer_addr;
            let state = self.state.clone();
            let duration = duration.clone();
            futures::lazy(move || {
                debug!("{}: connecting", peer_addr);
                state.borrow_mut().pending_conns += 1;
                sock.then(move |res| match res {
                    Err(e) => {
                        let mut s = state.borrow_mut();
                        s.consecutive_failures += 1;
                        s.pending_conns -= 1;
                        error!(
                            "{}: connection failed: {} [pending={}, failures={}]",
                            peer_addr,
                            e,
                            s.pending_conns,
                            s.consecutive_failures
                        );
                        Err(e)
                    }
                    Ok(sock) => {
                        {
                            let mut s = state.borrow_mut();
                            s.consecutive_failures = 0;
                            s.pending_conns -= 1;
                            s.open_conns += 1;
                            debug!(
                                "{}: connected [pending={}, open={}]",
                                peer_addr,
                                s.pending_conns,
                                s.open_conns
                            );
                        }

                        let ctx = Ctx {
                            state,
                            duration,
                            start: Instant::now(),
                        };
                        Ok(Connection::new(sock, ctx))
                    }
                })
            })
        };

        Connecting(Box::new(conn))
    }

    pub fn is_idle(&self) -> bool {
        self.state.borrow().is_idle()
    }
}

pub struct Connecting(Box<Future<Item = Connection, Error = io::Error> + 'static>);
impl Future for Connecting {
    type Item = Connection;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Connection, io::Error> {
        self.0.poll()
    }
}

pub struct Ctx {
    state: Rc<RefCell<State>>,
    duration: tacho::Timer,
    start: Instant,
}
impl ctx::Ctx for Ctx {
    fn read(&mut self, sz: usize) {
        let mut state = self.state.borrow_mut();
        state.rx_bytes += sz;
    }

    fn wrote(&mut self, sz: usize) {
        let mut state = self.state.borrow_mut();
        state.tx_bytes += sz;
    }
}
impl Drop for Ctx {
    fn drop(&mut self) {
        {
            let mut s = self.state.borrow_mut();
            s.open_conns -= 1;
            debug!("connection dropped [open={}]", s.open_conns);
        }
        self.duration.record_since(self.start)
    }
}
