//! Namerd Endpointer

// TODO In the future, we likely want to change this to use the split bind & addr APIs so
// balancers can be shared across logical names. In the meantime, it's sufficient to have
// a balancer per logical name.

use super::{WeightedAddr, Result, Error};
use bytes::{Buf, BufMut, IntoBuf, Bytes, BytesMut};
use futures::{Async, Future, IntoFuture, Poll, Stream};
use hyper::{Body, Chunk, Client, Uri};
use hyper::client::{Connect as HyperConnect, HttpConnector};
use hyper::status::StatusCode;
use serde_json as json;
use std::{f32, net, time};
use std::collections::HashMap;
use std::rc::Rc;
use tacho;
use tokio_core::reactor::Handle;
use tokio_timer::{Timer, Interval};
use url::Url;

type HttpConnectorFactory = Client<HttpConnector>;

type AddrsFuture = Box<Future<Item = Vec<WeightedAddr>, Error = Error>>;

// pub struct Addrs(Box<Stream<Item = Result<Vec<WeightedAddr>>, Error = ()>>);
// impl Stream for Addrs {
//     type Item = Result<Vec<WeightedAddr>>;
//     type Error = ();
//     fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
//         self.0.poll()
//     }
// }

#[derive(Clone)]
pub struct Namerd {
    base_url: String,
    period: time::Duration,
    namespace: String,
    stats: Stats,
}

impl Namerd {
    pub fn new(base_url: String,
               period: time::Duration,
               namespace: String,
               metrics: tacho::Scope)
               -> Namerd {
        Namerd {
            base_url: format!("{}/api/1/resolve/{}", base_url, namespace),
            stats: Stats::new(metrics),
            namespace,
            period,
        }
    }
}

impl Namerd {
    pub fn with_client(self, handle: &Handle, timer: &Timer) -> WithClient {
        WithClient {
            namerd: self,
            client: Rc::new(Client::new(handle)),
            timer: timer.clone(),
        }
    }
}

/// A name
pub struct WithClient {
    namerd: Namerd,
    client: Rc<HttpConnectorFactory>,
    timer: Timer,
}
impl WithClient {
    pub fn resolve(&self, target: &str) -> Addrs {
        let uri = Url::parse_with_params(&self.namerd.base_url, &[("path", &target)])
            .expect("invalid namerd url")
            .as_str()
            .parse::<Uri>()
            .expect("Could not parse namerd URI");
        let init = request(self.client.clone(), uri.clone(), self.namerd.stats.clone());
        let interval = self.timer.interval(self.namerd.period);
        Addrs {
            client: self.client.clone(),
            stats: self.namerd.stats.clone(),
            state: Some(State::Pending(init, interval)),
            uri,
        }
    }
}

/// Streams
pub struct Addrs {
    state: Option<State>,
    client: Rc<HttpConnectorFactory>,
    uri: Uri,
    stats: Stats,
}

enum State {
    Pending(AddrsFuture, Interval),
    Waiting(Interval),
}

impl Stream for Addrs {
    type Item = Result<Vec<WeightedAddr>>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match self.state.take().expect("polled after completion") {
                State::Waiting(mut int) => {
                    match int.poll() {
                        Err(e) => {
                            self.state = Some(State::Waiting(int));
                            return Err(Error::Timer(e));
                        }
                        Ok(Async::NotReady) => {
                            self.state = Some(State::Waiting(int));
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(_)) => {
                            let fut = {
                                let c = self.client.clone();
                                let u = self.uri.clone();
                                let s = self.stats.clone();
                                request(c, u, s)
                            };
                            self.state = Some(State::Pending(fut, int));
                        }
                    }
                }

                State::Pending(mut fut, int) => {
                    match fut.poll() {
                        Err(e) => {
                            self.state = Some(State::Waiting(int));
                            return Ok(Async::Ready(Some(Err(e))));
                        }
                        Ok(Async::Ready(addrs)) => {
                            self.state = Some(State::Waiting(int));
                            return Ok(Async::Ready(Some(Ok(addrs))));
                        }
                        Ok(Async::NotReady) => {
                            self.state = Some(State::Pending(fut, int));
                            return Ok(Async::NotReady);
                        }
                    }
                }
            }
        }
    }
}

fn request<C: HyperConnect>(client: Rc<Client<C>>, uri: Uri, stats: Stats) -> AddrsFuture {
    debug!("Polling namerd at {}", uri.to_string());
    let rsp = stats
        .request_latency
        .time(client.get(uri).then(handle_response))
        .then(move |rsp| {
                  if rsp.is_ok() {
                      stats.success_count.incr(1);
                  } else {
                      stats.failure_count.incr(1);
                  }
                  rsp
              });
    Box::new(rsp)
}

fn handle_response(result: ::hyper::Result<::hyper::client::Response>) -> AddrsFuture {
    match result {
        Ok(rsp) => {
            match rsp.status() {
                StatusCode::Ok => parse_body(rsp.body()),
                status => {
                    info!("error: bad response: {}", status);
                    Box::new(Err(Error::UnexpectedStatus(status)).into_future())
                }
            }
        }
        Err(e) => {
            error!("failed to read response: {:?}", e);
            Box::new(Err(Error::Hyper(e)).into_future())
        }
    }
}

fn parse_body(body: Body) -> AddrsFuture {
    trace!("parsing namerd response");
    body.collect()
        .then(|res| match res {
                  Ok(ref chunks) => parse_chunks(chunks),
                  Err(e) => {
                      info!("error: {}", e);
                      Err(Error::Hyper(e))
                  }
              })
        .boxed()
}

fn bytes_in(chunks: &[Chunk]) -> usize {
    let mut sz = 0;
    for c in chunks {
        sz += (*c).len();
    }
    sz
}

fn to_buf(chunks: &[Chunk]) -> Bytes {
    let mut buf = BytesMut::with_capacity(bytes_in(chunks));
    for c in chunks {
        buf.put_slice(&*c)
    }
    buf.freeze()
}

fn parse_chunks(chunks: &[Chunk]) -> Result<Vec<WeightedAddr>> {
    let r = to_buf(chunks).into_buf().reader();
    let result: json::Result<NamerdResponse> = json::from_reader(r);
    match result {
        Ok(ref nrsp) if nrsp.kind == "bound" => Ok(to_weighted_addrs(&nrsp.addrs)),
        Ok(_) => Err(Error::NotBound),
        Err(e) => {
            info!("error parsing response: {}", e);
            Err(Error::Serde(e))
        }
    }
}

fn to_weighted_addrs(namerd_addrs: &[NamerdAddr]) -> Vec<WeightedAddr> {
    // We never intentionally clear the EndpointMap.
    let mut dsts: Vec<WeightedAddr> = Vec::new();
    let mut sum = 0.0;
    for na in namerd_addrs {
        let addr = net::SocketAddr::new(na.ip.parse().unwrap(), na.port);
        let w = na.meta.endpoint_addr_weight.unwrap_or(1.0);
        sum += w;
        dsts.push(WeightedAddr::new(addr, w));
    }
    // Normalize weights on [0.0, 0.1].
    for mut dst in &mut dsts {
        dst.weight /= sum;
    }
    dsts
}

#[derive(Debug, Deserialize)]
struct NamerdResponse {
    #[serde(rename = "type")]
    kind: String,
    addrs: Vec<NamerdAddr>,
    meta: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct NamerdAddr {
    ip: String,
    port: u16,
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    authority: Option<String>,

    #[serde(rename = "nodeName")]
    node_name: Option<String>,

    endpoint_addr_weight: Option<f32>,
}


#[derive(Clone)]
pub struct Stats {
    request_latency: tacho::Timer,
    success_count: tacho::Counter,
    failure_count: tacho::Counter,
}
impl Stats {
    fn new(metrics: tacho::Scope) -> Stats {
        Stats {
            request_latency: metrics.timer_ms("request_latency_ms".into()),
            success_count: metrics.counter("success_count".into()),
            failure_count: metrics.counter("failure_count".into()),
        }
    }
}
