//! This library provides tools to build a HTTP proxy
//!
//! It handles network polling, HTTP parsing, TLS in a fast single threaded event
//! loop.
//!
//! It is designed to receive configuration changes at runtime instead of
//! reloading from a file regularly. The event loop runs in its own thread
//! and receives commands through a message queue.
//!
//! To create a HTTP proxy, you first need to create a `HttpProxyConfiguration`
//! structure (there are configuration structures for the HTTPS and TCP proxies
//! too).
//!
//! ```ignore
//! let config = proxy::HttpProxyConfiguration {
//!   front: "198.51.100.0:80".parse().unwrap(),
//!   ..Default::default()
//! };
//! ```
//!
//! Then create the required elements to communicate with the proxy thread,
//! and launch the thread:
//!
//! ```ignore
//! let config = proxy::HttpProxyConfiguration {
//!   front: "198.51.100.0:80".parse().unwrap(),
//!   ..Default::default()
//! };
//!
//! let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
//!
//! let jg            = thread::spawn(move || {
//!    network::http::start(config, channel);
//! });
//!
//! ```
//!
//! The `tx, rx` channel here is a mio channel through which the proxy will
//! receive its orders, and it will use the `(sender,rec)` one to send
//! acknowledgements and various data.
//!
//! Once the thread is launched, the proxy will start its event loop and handle
//! events on the listening interface and port specified in the configuration
//! object. Since no applications to proxy for were specified, it will receive
//! the connections, parse the request, then send a default (but configurable)
//! answer.
//!
//! ```ignore
//! let http_front = proxy::HttpFront {
//!   app_id:     String::from("test"),
//!   hostname:   String::from("example.com"),
//!   path_begin: String::from("/")
//! };
//! let http_backend = proxy::Backend {
//!   app_id:     String::from("test"),
//!   ip_address: String::from("192.0.2.1"),
//!   port:       8080
//! };
//!
//! command.write_message(&proxy::ProxyRequest {
//!   id:    String::from("ID_ABCD"),
//!   order: proxy::ProxyRequestData::AddHttpFront(http_front)
//! ));
//!
//! command.write_message(&proxy::ProxyRequest {
//!   id:    String::from("ID_EFGH"),
//!   order: proxy::ProxyRequestData::AddBackend(http_backend)
//! ));
//!
//! println!("HTTP -> {:?}", command.read_message());
//! println!("HTTP -> {:?}", command.read_message());
//! ```
//!
//! An application is identified by its `app_id`, a string that will be shared
//! between one or multiple "fronts", and one or multiple "backends".
//!
//! A "front" is a way to recognize a request and match it to an `app_id`,
//! depending on the hostname and the beginning of the URL path.
//!
//! A backend corresponds to one backend server, indicated by its IP and port.
//!
//! An application can have multiple backend servers, and they can be added or
//! removed while the proxy is running. If a backend is removed from the configuration
//! while the proxy is handling a request to that server, it will finish that
//! request and stop sending new traffic to that server.
//!
//! The fronts and backends are specified with messages sent through the
//! communication channels with the proxy event loop. Once the configuration
//! options are added to the proxy's state, it will send back an acknowledgement
//! message.
//!
//! Here is the complete example for reference:
//!
//! ```ignore
//! #[macro_use] extern crate log;
//! extern crate env_logger;
//! extern crate sozu_lib as sozu;
//! extern crate sozu_command_lib as sozu_command;
//! extern crate openssl;
//! extern crate mio;
//!
//! use std::thread;
//! use std::sync::mpsc;
//! use sozu_command::messages;
//! use sozu_command::channel::Channel;
//! use sozu::network;
//!
//! fn main() {
//!   env_logger::init().unwrap();
//!   info!("starting up");
//!
//!   let config = proxy::HttpProxyConfiguration {
//!     front: "198.51.100.0:80".parse().unwrap(),
//!     ..Default::default()
//!   };
//!
//!   let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
//!
//!   let jg            = thread::spawn(move || {
//!      network::http::start(config, channel);
//!   });
//!
//!   let http_front = proxy::HttpFront {
//!     app_id:     String::from("test"),
//!     hostname:   String::from("example.com"),
//!     path_begin: String::from("/")
//!   };
//!   let http_backend = proxy::Backend {
//!     app_id:     String::from("test"),
//!     ip_address: String::from("192.0.2.1"),
//!     port:       8080
//!   };
//!
//!   command.write_message(&proxy::ProxyRequest {
//!     id:    String::from("ID_ABCD"),
//!     order: proxy::ProxyRequestData::AddHttpFront(http_front)
//!   ));
//!
//!   command.write_message(&proxy::ProxyRequest {
//!     id:    String::from("ID_EFGH"),
//!     order: proxy::ProxyRequestData::AddBackend(http_backend)
//!   ));
//!
//!   println!("HTTP -> {:?}", command.read_message());
//!   println!("HTTP -> {:?}", command.write_message());
//!
//!   let _ = jg.join();
//!   info!("good bye");
//! }
//! ```
//!
#![cfg_attr(feature = "unstable", feature(test))]
#[cfg(all(feature = "unstable", test))]
extern crate test;

#[macro_use] extern crate nom;
extern crate mio;
extern crate url;
extern crate log;
extern crate time;
extern crate rand;
#[cfg(feature = "use-openssl")]
extern crate openssl;
extern crate rustls;
extern crate pool as pool_crate;
extern crate uuid;
extern crate net2;
extern crate libc;
extern crate slab;
extern crate mio_uds;
extern crate hdrhistogram;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate idna;
extern crate webpki;
extern crate mio_extras;
extern crate hashbrown;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
#[cfg(feature = "use-openssl")]
extern crate openssl_sys;
extern crate chrono;

#[macro_use] pub mod util;
#[macro_use] pub mod metrics;

pub mod pool;
pub mod buffer_queue;
pub mod socket;
pub mod router;
pub mod protocol;
pub mod http;
pub mod backends;
pub mod retry;
pub mod load_balancing;
pub mod features;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;
pub mod server;

#[cfg(feature = "use-openssl")]
pub mod https_openssl;

pub mod https_rustls;

use mio::{Poll,Ready,Token};
use mio::unix::UnixReady;
use mio::net::TcpStream;
use std::fmt;
use std::str;
use std::net::SocketAddr;
use std::rc::Rc;
use std::cell::RefCell;
use time::{SteadyTime,Duration};
use mio_extras::timer::{Timer,Timeout};

use sozu_command::proxy::{ProxyRequest,ProxyResponse,LoadBalancingParams};

use self::retry::RetryPolicy;

pub type AppId = String;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum Protocol {
  HTTP,
  HTTPS,
  TCP,
  HTTPListen,
  HTTPSListen,
  TCPListen,
  Channel,
  Metrics,
  Timer,
}

#[derive(Debug,Clone,Default)]
pub struct CloseResult {
  pub tokens:   Vec<Token>,
}

pub trait ProxySession {
  fn protocol(&self)  -> Protocol;
  fn ready(&mut self) -> SessionResult;
  fn process_events(&mut self, token: Token, events: Ready);
  fn close(&mut self, poll: &mut Poll) -> CloseResult;
  fn close_backend(&mut self, token: Token, poll: &mut Poll);
  fn timeout(&mut self, t: Token, timer: &mut Timer<Token>, front_timeout: &Duration) -> SessionResult;
  fn cancel_timeouts(&self, timer: &mut Timer<Token>);
  fn last_event(&self) -> SteadyTime;
  fn print_state(&self);
  fn tokens(&self) -> Vec<Token>;
  fn shutting_down(&mut self) -> SessionResult;
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub enum BackendConnectionStatus {
  NotConnected,
  Connecting,
  Connected,
}

#[derive(Debug,PartialEq)]
pub enum BackendConnectAction {
  New,
  Reuse,
  Replace,
}

#[derive(Debug,PartialEq)]
pub enum AcceptError {
  IoError,
  TooManySessions,
  WouldBlock,
}

use self::server::{ListenToken,ListenPortState};
pub trait ProxyConfiguration<Session> {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, session: &mut Session,
    back_token: Token) ->Result<BackendConnectAction,ConnectionError>;
  fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse;
  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError>;
  fn create_session(&mut self, socket: TcpStream, token: ListenToken, event_loop: &mut Poll, session_token: Token, timeout: Timeout)
    -> Result<(Rc<RefCell<Session>>, bool), AcceptError>;
  fn listen_port_state(&self, port: &u16) -> ListenPortState;
}

#[derive(Debug,PartialEq,Eq)]
pub enum RequiredEvents {
  FrontReadBackNone,
  FrontWriteBackNone,
  FrontReadWriteBackNone,
  FrontNoneBackNone,
  FrontReadBackRead,
  FrontWriteBackRead,
  FrontReadWriteBackRead,
  FrontNoneBackRead,
  FrontReadBackWrite,
  FrontWriteBackWrite,
  FrontReadWriteBackWrite,
  FrontNoneBackWrite,
  FrontReadBackReadWrite,
  FrontWriteBackReadWrite,
  FrontReadWriteBackReadWrite,
  FrontNoneBackReadWrite,
}

impl RequiredEvents {

  pub fn front_readable(&self) -> bool {
    match *self {
      RequiredEvents::FrontReadBackNone
      | RequiredEvents:: FrontReadWriteBackNone
      | RequiredEvents:: FrontReadBackRead
      | RequiredEvents:: FrontReadWriteBackRead
      | RequiredEvents:: FrontReadBackWrite
      | RequiredEvents:: FrontReadWriteBackWrite
      | RequiredEvents:: FrontReadBackReadWrite
      | RequiredEvents:: FrontReadWriteBackReadWrite => true,
      _ => false
    }
  }

  pub fn front_writable(&self) -> bool {
    match *self {
        RequiredEvents::FrontWriteBackNone
        | RequiredEvents::FrontReadWriteBackNone
        | RequiredEvents::FrontWriteBackRead
        | RequiredEvents::FrontReadWriteBackRead
        | RequiredEvents::FrontWriteBackWrite
        | RequiredEvents::FrontReadWriteBackWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite => true,
        _ => false
    }
  }

  pub fn back_readable(&self) -> bool {
    match *self {
        RequiredEvents::FrontReadBackRead
        | RequiredEvents::FrontWriteBackRead
        | RequiredEvents::FrontReadWriteBackRead
        | RequiredEvents::FrontNoneBackRead
        | RequiredEvents::FrontReadBackReadWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite
        | RequiredEvents::FrontNoneBackReadWrite => true,
        _ => false
    }
  }

  pub fn back_writable(&self) -> bool {
    match *self {
        RequiredEvents::FrontReadBackWrite
        | RequiredEvents::FrontWriteBackWrite
        | RequiredEvents::FrontReadWriteBackWrite
        | RequiredEvents::FrontNoneBackWrite
        | RequiredEvents::FrontReadBackReadWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite
        | RequiredEvents::FrontNoneBackReadWrite => true,
        _ => false
    }
  }
}

#[derive(Debug,PartialEq,Eq)]
pub enum SessionResult {
  CloseSession,
  CloseBackend(Option<Token>),
  ReconnectBackend(Option<Token>, Option<Token>),
  Continue,
  ConnectBackend
}

#[derive(Debug,PartialEq,Eq)]
pub enum ConnectionError {
  NoHostGiven,
  NoRequestLineGiven,
  InvalidHost,
  HostNotFound,
  NoBackendAvailable,
  ToBeDefined,
  HttpsRedirect
}

#[derive(Debug,PartialEq,Eq)]
pub enum SocketType {
  Listener,
  FrontClient
}

#[derive(Debug,PartialEq,Eq,Clone)]
pub enum BackendStatus {
  Normal,
  Closing,
  Closed,
}

#[derive(Debug,PartialEq,Eq,Clone)]
pub struct Backend {
  pub sticky_id:                 Option<String>,
  pub backend_id:                String,
  pub address:                   SocketAddr,
  pub status:                    BackendStatus,
  pub retry_policy:              retry::RetryPolicyWrapper,
  pub active_connections:        usize,
  pub failures:                  usize,
  pub load_balancing_parameters: Option<LoadBalancingParams>,
  pub backup:                    bool,
}

impl Backend {
  pub fn new(backend_id: &str, address: SocketAddr, sticky_id: Option<String>, load_balancing_parameters: Option<LoadBalancingParams>, backup: Option<bool>) -> Backend {
    let desired_policy = retry::ExponentialBackoffPolicy::new(6);
    Backend {
      sticky_id,
      backend_id:         backend_id.to_string(),
      address,
      status:             BackendStatus::Normal,
      retry_policy:       desired_policy.into(),
      active_connections: 0,
      failures:           0,
      load_balancing_parameters,
      backup: backup.unwrap_or(false),
    }
  }

  pub fn set_closing(&mut self) {
    self.status = BackendStatus::Closing;
  }

  pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
    &mut self.retry_policy
  }

  pub fn can_open(&self) -> bool {
    if let Some(action) = self.retry_policy.can_try() {
      self.status == BackendStatus::Normal && action == retry::RetryAction::OKAY
    } else {
      false
    }
  }

  pub fn inc_connections(&mut self) -> Option<usize> {
    if self.status == BackendStatus::Normal {
      self.active_connections += 1;
      Some(self.active_connections)
    } else {
      None
    }
  }

  pub fn dec_connections(&mut self) -> Option<usize> {
    match self.status {
      BackendStatus::Normal => {
        if self.active_connections > 0 {
          self.active_connections -= 1;
        }
        Some(self.active_connections)
      }
      BackendStatus::Closed  => None,
      BackendStatus::Closing => {
        if self.active_connections > 0 {
          self.active_connections -= 1;
        }
        if self.active_connections == 0 {
          self.status = BackendStatus::Closed;
          None
        } else {
          Some(self.active_connections)
        }
      },
    }
  }

  pub fn try_connect(&mut self) -> Result<mio::tcp::TcpStream, ConnectionError> {
    if self.status != BackendStatus::Normal {
      return Err(ConnectionError::NoBackendAvailable);
    }

    //FIXME: what happens if the connect() call fails with EINPROGRESS?
    let conn = mio::tcp::TcpStream::connect(&self.address).map_err(|_| ConnectionError::NoBackendAvailable);
    if conn.is_ok() {
      //self.retry_policy.succeed();
      self.inc_connections();
    } else {
      self.retry_policy.fail();
      self.failures += 1;
    }

    conn
  }
}

#[derive(Clone)]
pub struct Readiness {
  pub event:    UnixReady,
  pub interest: UnixReady,
}

impl Readiness {
  pub fn new() -> Readiness {
    Readiness {
      event:    UnixReady::from(Ready::empty()),
      interest: UnixReady::from(Ready::empty()),
    }
  }

  pub fn reset(&mut self) {
    self.event =  UnixReady::from(Ready::empty());
    self.interest  = UnixReady::from(Ready::empty());
  }
}

pub fn display_unix_ready(s: &mut [u8], readiness: UnixReady) {
  if readiness.is_readable() {
    s[0] = b'R';
  }
  if readiness.is_writable() {
    s[1] = b'W';
  }
  if readiness.is_error() {
    s[2] = b'E';
  }
  if readiness.is_hup() {
    s[3] = b'H';
  }
}

pub fn unix_ready_to_string(readiness: UnixReady) -> String {
  let s = &mut [b'-'; 4];
  display_unix_ready(s, readiness);
  String::from_utf8(s.to_vec()).unwrap()
}

impl fmt::Debug for Readiness {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {

    let i = &mut [b'-'; 4];
    let r = &mut [b'-'; 4];
    let mixed = &mut [b'-'; 4];

    display_unix_ready(i, self.interest);
    display_unix_ready(r, self.event);
    display_unix_ready(mixed, self.interest & self.event);

    write!(f, "Readiness {{ interest: {}, readiness: {}, mixed: {} }}",
      str::from_utf8(i).unwrap(),
      str::from_utf8(r).unwrap(),
      str::from_utf8(mixed).unwrap())
  }
}

#[derive(Clone,Debug)]
pub struct SessionMetrics {
  /// date at which we started handling that request
  pub start:        Option<SteadyTime>,
  /// time actually spent handling the request
  pub service_time: Duration,
  /// bytes received by the frontend
  pub bin:          usize,
  /// bytes sent by the frontend
  pub bout:         usize,

  /// date at which we started working on the request
  pub service_start: Option<SteadyTime>,

  pub backend_id:    Option<String>,
  pub backend_start: Option<SteadyTime>,
  pub backend_connected: Option<SteadyTime>,
  pub backend_stop:  Option<SteadyTime>,
  pub backend_bin:   usize,
  pub backend_bout:  usize,
}

impl SessionMetrics {
  pub fn new() -> SessionMetrics {
    SessionMetrics {
      start:         Some(SteadyTime::now()),
      service_time:  Duration::seconds(0),
      bin:           0,
      bout:          0,
      service_start: None,
      backend_id:    None,
      backend_start: None,
      backend_connected: None,
      backend_stop:  None,
      backend_bin:   0,
      backend_bout:  0,
    }
  }

  pub fn reset(&mut self) {
    self.start         = None;
    self.service_time  = Duration::seconds(0);
    self.bin           = 0;
    self.bout          = 0;
    self.service_start = None;
    self.backend_start = None;
    self.backend_connected = None;
    self.backend_stop  = None;
    self.backend_bin   = 0;
    self.backend_bout  = 0;
  }

  pub fn service_start(&mut self) {
    if self.start.is_none() {
      self.start = Some(SteadyTime::now());
    }

    self.service_start = Some(SteadyTime::now());
  }

  pub fn service_stop(&mut self) {
    if self.service_start.is_some() {
      let start = self.service_start.take().unwrap();
      let duration = SteadyTime::now() - start;
      self.service_time = self.service_time + duration;
    }
  }

  pub fn service_time(&self) -> Duration {
    match self.service_start {
      Some(start) => {
        let last_duration = SteadyTime::now() - start;
        self.service_time + last_duration
      },
      None        => self.service_time,
    }
  }

  pub fn response_time(&self) -> Duration {
    match self.start {
      Some(start) => SteadyTime::now() - start,
      None        => Duration::seconds(0),
    }
  }

  pub fn backend_start(&mut self) {
    self.backend_start = Some(SteadyTime::now());
  }

  pub fn backend_connected(&mut self) {
    self.backend_connected = Some(SteadyTime::now());
  }

  pub fn backend_stop(&mut self) {
    self.backend_stop = Some(SteadyTime::now());
  }

  pub fn backend_response_time(&self) -> Option<Duration> {
    match (self.backend_connected, self.backend_stop) {
      (Some(start), Some(end)) => {
        Some(end - start)
      },
      (Some(start), None) => Some(SteadyTime::now() - start),
      _ => None
    }
  }

  pub fn backend_connection_time(&self) -> Option<Duration> {
    match (self.backend_start, self.backend_connected) {
      (Some(start), Some(end)) => {
        Some(end - start)
      },
      _ => None
    }
  }
}

pub struct LogDuration(Duration);

impl fmt::Display for LogDuration {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let secs = self.0.num_seconds();
    if secs >= 10 {
      return write!(f, "{}s", secs);
    }

    let ms = self.0.num_milliseconds();

    if ms < 10 {
      if let Some(us) = self.0.num_microseconds() {
        if us >= 10 {
          return write!(f, "{}μs", us);
        }

        if let Some(ns) = self.0.num_nanoseconds() {
          return write!(f, "{}ns", ns);
        }
      }
    }

    write!(f, "{}ms", ms)
  }
}

