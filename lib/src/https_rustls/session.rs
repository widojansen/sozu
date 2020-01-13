use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::net::{Shutdown,SocketAddr};
use mio::*;
use mio::net::*;
use mio::unix::UnixReady;
use std::io::{ErrorKind,Read};
use time::{SteadyTime, Duration};
use uuid::Uuid;
use rustls::{ServerSession,Session as ClientSession,ProtocolVersion,SupportedCipherSuite,CipherSuite};
use mio_extras::timer::{Timer, Timeout};
use sozu_command::buffer::fixed::Buffer;
use sozu_command::proxy::ProxyEvent;

use protocol::http::parser::RequestState;
use pool::Pool;
use {Backend,SessionResult,Protocol,Readiness,SessionMetrics, ProxySession,
  BackendConnectionStatus, CloseResult};
use socket::FrontRustls;
use protocol::{ProtocolResult,Http,Pipe};
use protocol::rustls::TlsHandshake;
use protocol::http::{DefaultAnswerStatus, TimeoutStatus, answers::HttpAnswers};
use protocol::proxy_protocol::expect::ExpectProxyProtocol;
use retry::RetryPolicy;
use util::UnwrapLog;
use buffer_queue::BufferQueue;
use server::push_event;

pub enum State {
  Expect(ExpectProxyProtocol<TcpStream>, ServerSession),
  Handshake(TlsHandshake),
  Http(Http<FrontRustls>),
  WebSocket(Pipe<FrontRustls>)
}

pub struct Session {
  pub frontend_token: Token,
  pub backend:        Option<Rc<RefCell<Backend>>>,
  pub back_connected: BackendConnectionStatus,
  protocol:           Option<State>,
  pub public_address: Option<SocketAddr>,
  pool:               Weak<RefCell<Pool<Buffer>>>,
  pub metrics:        SessionMetrics,
  pub app_id:         Option<String>,
  sticky_name:        String,
  timeout:            Timeout,
  last_event:         SteadyTime,
  pub listen_token:   Token,
  pub connection_attempt: u8,
  peer_address:       Option<SocketAddr>,
  answers:            Rc<RefCell<HttpAnswers>>,
}

impl Session {
  pub fn new(ssl: ServerSession, sock: TcpStream, token: Token, pool: Weak<RefCell<Pool<Buffer>>>,
    public_address: Option<SocketAddr>, expect_proxy: bool, sticky_name: String, timeout: Timeout,
    answers: Rc<RefCell<HttpAnswers>>, listen_token: Token) -> Session {
    let peer_address = if expect_proxy {
      // Will be defined later once the expect proxy header has been received and parsed
      None
    } else {
      sock.peer_addr().ok()
    };

    let request_id = Uuid::new_v4().to_hyphenated();
    let state = if expect_proxy {
      trace!("starting in expect proxy state");
      gauge_add!("protocol.proxy.expect", 1);
      Some(State::Expect(ExpectProxyProtocol::new(sock, token, request_id), ssl))
    } else {
      gauge_add!("protocol.tls.handshake", 1);
      Some(State::Handshake(TlsHandshake::new(ssl, sock, request_id)))
    };

    let mut session = Session {
      frontend_token: token,
      backend:        None,
      back_connected: BackendConnectionStatus::NotConnected,
      protocol:       state,
      public_address,
      pool,
      metrics:        SessionMetrics::new(),
      app_id:         None,
      sticky_name,
      timeout,
      last_event:     SteadyTime::now(),
      listen_token,
      connection_attempt: 0,
      peer_address,
      answers,
    };
    session.front_readiness().interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
    session
  }

  pub fn http(&mut self) -> Option<&mut Http<FrontRustls>> {
    self.protocol.as_mut().and_then(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        Some(http)
      } else {
        None
      }
    })
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Rc<Vec<u8>>)  {
    self.protocol.as_mut().map(|protocol| {
      if let State::Http(ref mut http) = *protocol {
        http.set_answer(answer, buf);
      }
    });
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = unwrap_msg!(self.protocol.take());

    if let State::Expect(expect, ssl) = protocol {
      debug!("switching to TLS handshake");
      if let Some(ref addresses) = expect.addresses {
        if let (Some(public_address), Some(session_address)) = (addresses.destination(), addresses.source()) {
          self.public_address = Some(public_address);
          self.peer_address = Some(session_address);

          let ExpectProxyProtocol {
            frontend, readiness, request_id, .. } = expect;

          let mut tls = TlsHandshake::new(ssl, frontend, request_id);
          tls.readiness.event = readiness.event;
          tls.readiness.event.insert(Ready::readable());

          gauge_add!("protocol.proxy.expect", -1);
          gauge_add!("protocol.tls.handshake", 1);
          self.protocol = Some(State::Handshake(tls));
          return true;
        }
      }

      error!("failed to upgrade from expect");
      self.protocol = Some(State::Expect(expect, ssl));
      false
    } else if let State::Handshake(handshake) = protocol {
      let front_buf = self.pool.upgrade().and_then(|p| p.borrow_mut().checkout());
      if front_buf.is_none() {
        self.protocol = Some(State::Handshake(handshake));
        return false;
      }

      let mut front_buf = front_buf.unwrap();

      handshake.session.get_protocol_version().map(|version| {
        incr!(version_str(version));
      });
      handshake.session.get_negotiated_ciphersuite().map(|cipher| {
        incr!(ciphersuite_str(cipher));
      });

      let front_stream = FrontRustls {
        stream:  handshake.stream,
        session: handshake.session,
      };

      let readiness = handshake.readiness.clone();
      let http = Http::new(front_stream, self.frontend_token, handshake.request_id,
        self.pool.clone(), self.public_address, self.peer_address,
        self.sticky_name.clone(), Protocol::HTTPS).map(|mut http| {

        let res = http.frontend.session.read(front_buf.space());
        match res {
          Ok(sz) =>{
            //info!("rustls upgrade: there were {} bytes of plaintext available", sz);
            front_buf.fill(sz);
            count!("bytes_in", sz as i64);
            self.metrics.bin += sz;
          },
          Err(e) => {
            error!("read error: {:?}", e);
          }
        }

        let sz = front_buf.available_data();
        let mut buf = BufferQueue::with_buffer(front_buf);
        buf.sliced_input(sz);

        gauge_add!("protocol.tls.handshake", -1);
        gauge_add!("protocol.https", 1);
        http.front_buf = Some(buf);
        http.front_readiness = readiness;
        http.front_readiness.interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
        State::Http(http)
      });

      if http.is_none() {
        error!("could not upgrade to HTTP");
        //we cannot put back the protocol since we moved the stream
        //self.protocol = Some(State::Handshake(handshake));
        return false;
      }

      self.protocol = http;
      return true;
    } else if let State::Http(http) = protocol {
      debug!("https switching to wss");
      let front_token = self.frontend_token;
      let back_token  = unwrap_msg!(http.back_token());


      let front_buf = match http.front_buf {
        Some(buf) => buf.buffer,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };
      let back_buf = match http.back_buf {
        Some(buf) => buf.buffer,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };

      let mut pipe = Pipe::new(http.frontend, front_token, http.request_id,
        http.backend, front_buf, back_buf, http.session_address, Protocol::HTTPS);

      pipe.front_readiness.event = http.front_readiness.event;
      pipe.back_readiness.event  = http.back_readiness.event;
      pipe.set_back_token(back_token);
      pipe.set_app_id(self.app_id.clone());

      gauge_add!("protocol.https", -1);
      gauge_add!("protocol.wss", 1);
      gauge_add!("http.active_requests", -1);
      self.protocol = Some(State::WebSocket(pipe));
      true
    } else {
      self.protocol = Some(protocol);
      true
    }
  }

  fn front_hup(&mut self)     -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.front_hup(),
      State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
      State::Handshake(_)            => {
        SessionResult::CloseSession
      },
      State::Expect(_,_)             => {
        SessionResult::CloseSession
      }
    }
  }

  fn back_hup(&mut self)      -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
      State::Handshake(_)            => {
        error!("why a backend HUP event while still in frontend handshake?");
        SessionResult::CloseSession
      },
      State::Expect(_,_)             => {
        error!("why a backend HUP event while still in frontend proxy protocol expect?");
        SessionResult::CloseSession
      }
    }
  }

  pub fn log_context(&self)  -> String {
    if let State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.log_context()
    } else {
      "".to_string()
    }
  }

  fn readable(&mut self)      -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect, _)    => expect.readable(&mut self.metrics),
      State::Handshake(ref mut handshake) => handshake.readable(),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else if self.upgrade() {
      self.readable()
    } else {
      SessionResult::CloseSession
    }
  }

  fn writable(&mut self)      -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)                  => return SessionResult::CloseSession,
      State::Handshake(ref mut handshake) => handshake.writable(),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.writable(&mut self.metrics)),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.writable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else if self.upgrade() {
      if (self.front_readiness().event & self.front_readiness().interest).is_writable() {
        self.writable()
      } else {
        SessionResult::Continue
      }
    } else {
      SessionResult::CloseSession
    }
  }

  fn back_readable(&mut self) -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)             => return SessionResult::CloseSession,
      State::Http(ref mut http)      => http.back_readable(&mut self.metrics),
      State::Handshake(_)            => (ProtocolResult::Continue, SessionResult::CloseSession),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else if self.upgrade() {
      match *unwrap_msg!(self.protocol.as_mut()) {
        State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
        _ => result
      }
    } else {
      SessionResult::CloseSession
    }
  }

  fn back_writable(&mut self) -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)                  => SessionResult::CloseSession,
      State::Handshake(_)                 => SessionResult::CloseSession,
      State::Http(ref mut http)           => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.back_writable(&mut self.metrics),
    }
  }

  pub fn front_socket(&self) -> &TcpStream {
    match unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(ref expect,_)     => expect.front_socket(),
      State::Handshake(ref handshake) => &handshake.stream,
      State::Http(ref http)           => http.front_socket(),
      State::WebSocket(ref pipe)      => pipe.front_socket(),
    }
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(_,_)         => None,
      State::Handshake(_)        => None,
      State::Http(ref http)      => http.back_socket(),
      State::WebSocket(ref pipe) => pipe.back_socket(),
    }
  }

  pub fn back_token(&self)   -> Option<Token> {
    match unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(_,_)         => None,
      State::Handshake(_)        => None,
      State::Http(ref http)      => http.back_token(),
      State::WebSocket(ref pipe) => pipe.back_token(),
    }
  }

  pub fn set_back_socket(&mut self, sock:TcpStream) {
    if let State::Http(ref mut http) = unwrap_msg!(self.protocol.as_mut()) {
      http.set_back_socket(sock)
    }
  }

  pub fn set_back_token(&mut self, token: Token) {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_back_token(token),
      State::WebSocket(ref mut pipe) => pipe.set_back_token(token),
      _ => {}
    }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;

    if connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", 1, self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
      self.backend.as_ref().map(|backend| {
        let backend = &mut (*backend.borrow_mut());
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) {
    if let Some(backend) = self.backend.take() {
      self.http().map(|h| h.clear_back_token());

      (*backend.borrow_mut()).dec_connections();
    }
  }

  pub fn front_readiness(&mut self)      -> &mut Readiness {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect, _)    => &mut expect.readiness,
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.front_readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.front_readiness,
    }
  }

  pub fn back_readiness(&mut self)      -> Option<&mut Readiness> {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)           => Some(http.back_readiness()),
      State::WebSocket(ref mut pipe)      => Some(&mut pipe.back_readiness),
      _ => None,
    }
  }

  fn fail_backend_connection(&mut self) {
    self.backend.as_ref().map(|backend| {
      let ref mut backend = *backend.borrow_mut();
      backend.failures += 1;

      let already_unavailable = backend.retry_policy.is_down();
      backend.retry_policy.fail();
      incr!("connections.error", self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
      if !already_unavailable && backend.retry_policy.is_down() {
        error!("backend server {} at {} is down", backend.backend_id, backend.address);
        incr!("down", self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));

        push_event(ProxyEvent::BackendDown(backend.backend_id.clone(), backend.address));
      }
    });
  }

  fn reset_connection_attempt(&mut self) {
    self.connection_attempt = 0;
  }
}

impl ProxySession for Session {
  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
    self.http().map(|http| http.close());
    self.metrics.service_stop();
    if let Err(e) = self.front_socket().shutdown(Shutdown::Both) {
      if e.kind() != ErrorKind::NotConnected {
        error!("error closing front socket: {:?}", e);
      }
    }

    if let Err(e) = poll.deregister(self.front_socket()) {
      error!("error deregistering front socket: {:?}", e);
    }

    let mut result = CloseResult::default();

    if let Some(tk) = self.back_token() {
      result.tokens.push(tk)
    }

    //FIXME: should we really pass a token here?
    self.close_backend(Token(0), poll);

    if let Some(State::Http(ref http)) = self.protocol {
      //if the state was initial, the connection was already reset
      if http.request != Some(RequestState::Initial) {
        gauge_add!("http.active_requests", -1);
      }
    }

    match self.protocol {
      Some(State::Expect(_,_)) => gauge_add!("protocol.proxy.expect", -1),
      Some(State::Handshake(_)) => gauge_add!("protocol.tls.handshake", -1),
      Some(State::Http(_)) => gauge_add!("protocol.https", -1),
      Some(State::WebSocket(_)) => gauge_add!("protocol.wss", -1),
      None => {},
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn timeout(&mut self, token: Token, timer: &mut Timer<Token>, front_timeout: &Duration) -> SessionResult {
    if self.frontend_token == token {
      let dur = SteadyTime::now() - self.last_event;
      if dur < *front_timeout {
        timer.set_timeout((*front_timeout - dur).to_std().unwrap(), token);
        SessionResult::Continue
      } else {
        match self.http().map(|h| h.timeout_status()) {
          Some(TimeoutStatus::Request) => {
            let answer = self.answers.borrow().get(DefaultAnswerStatus::Answer408, None);
            self.set_answer(DefaultAnswerStatus::Answer408, answer);
            self.writable()
          },
          Some(TimeoutStatus::Response) => {
            let answer = self.answers.borrow().get(DefaultAnswerStatus::Answer504, None);
            self.set_answer(DefaultAnswerStatus::Answer504, answer);
            self.writable()
          },
          _ => {
            SessionResult::CloseSession
          }
        }
      }
    } else {
      //invalid token, obsolete timeout triggered
      SessionResult::Continue
    }
  }

  fn cancel_timeouts(&self, timer: &mut Timer<Token>) {
    timer.cancel_timeout(&self.timeout);
  }

  fn close_backend(&mut self, _: Token, poll: &mut Poll) {
    self.remove_backend();

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
      self.back_readiness().map(|r| r.event = UnixReady::from(Ready::empty()));
      if let Some(sock) = self.back_socket() {
        if let Err(e) = sock.shutdown(Shutdown::Both) {
          if e.kind() != ErrorKind::NotConnected {
            error!("error shutting down backend socket: {:?}", e);
          }
        }

        if let Err(e) = poll.deregister(sock) {
          error!("error deregistering backend socket: {:?}", e);
        }
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", -1, self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);
    self.http().map(|h| h.clear_back_token());
    self.http().map(|h| h.remove_backend());
  }

  fn protocol(&self) -> Protocol {
    Protocol::HTTPS
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    trace!("token {:?} got event {}", token, super::super::unix_ready_to_string(UnixReady::from(events)));
    self.last_event = SteadyTime::now();

    if self.frontend_token == token {
      self.front_readiness().event = self.front_readiness().event | UnixReady::from(events);
    } else if self.back_token() == Some(token) {
      self.back_readiness().map(|r| r.event = r.event | UnixReady::from(events));
    }
  }

  fn ready(&mut self) -> SessionResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting &&
      self.back_readiness().map(|r| r.event != UnixReady::from(Ready::empty())).unwrap_or(false) {

      if self.back_readiness().map(|r| r.event.is_hup()).unwrap_or(false) ||
        !self.http().map(|h| h.test_back_socket()).unwrap_or(false) {

        //retry connecting the backend
        error!("{} error connecting to backend, trying again", self.log_context());
        self.metrics().service_stop();
        self.connection_attempt += 1;
        self.fail_backend_connection();

        let backend_token = self.back_token();
        return SessionResult::ReconnectBackend(Some(self.frontend_token), backend_token);
      } else {
        self.metrics().backend_connected();
        self.reset_connection_attempt();
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    if self.front_readiness().event.is_hup() {
      let order = self.front_hup();
      match order {
        SessionResult::CloseSession => {
          return order;
        },
        _ => {
          self.front_readiness().event.remove(UnixReady::hup());
          return order;
        }
      }
    }

    let token = self.frontend_token;
    while counter < max_loop_iterations {
      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event).unwrap_or(UnixReady::from(Ready::empty()));

      trace!("PROXY\t{} {:?} F:{:?} B:{:?}", self.log_context(), token, self.front_readiness().clone(), self.back_readiness());

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if self.back_readiness().map(|r| r.event.is_hup()).unwrap_or(false) && self.front_readiness().interest.is_writable() &&
        ! self.front_readiness().event.is_writable() {
        break;
      }

      if front_interest.is_readable() {
        let order = self.readable();
        trace!("front readable\tinterpreting session order {:?}", order);

        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_writable() {
        let order = self.back_writable();
        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_readable() {
        let order = self.back_readable();
        if order != SessionResult::Continue {
          return order;
        }
      }

      if front_interest.is_writable() {
        let order = self.writable();
        trace!("front writable\tinterpreting session order {:?}", order);
        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          SessionResult::CloseSession => {
            return order;
          },
          SessionResult::Continue => {},
          _ => {
            self.back_readiness().map(|r| r.event.remove(UnixReady::hup()));
            return order;
          }
        };
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY session {:?} front error, disconnecting", self.frontend_token);
        } else {
          error!("PROXY session {:?} back error, disconnecting", self.frontend_token);
        }

        self.front_readiness().interest = UnixReady::from(Ready::empty());
        self.back_readiness().map(|r| r.interest  = UnixReady::from(Ready::empty()));
        return SessionResult::CloseSession;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);
      incr!("https_rustls.infinite_loop.error");

      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event);

      let token = self.frontend_token;
      let back = self.back_readiness().cloned();
      error!("PROXY\t{:?} readiness: front {:?} / back {:?} |front: {:?} | back: {:?} ", token,
        self.front_readiness(), back, front_interest, back_interest);
      self.print_state();

      return SessionResult::CloseSession;
    }

    SessionResult::Continue
  }

  fn shutting_down(&mut self) -> SessionResult {
    match &mut self.protocol {
      Some(State::Http(h)) => h.shutting_down(),
      Some(State::Handshake(_)) => SessionResult::Continue,
      _    => SessionResult::CloseSession,
    }
  }

  fn last_event(&self) -> SteadyTime {
    self.last_event
  }

  fn print_state(&self) {
    let p:String = match &self.protocol {
      Some(State::Expect(_,_))  => String::from("Expect"),
      Some(State::Handshake(_)) => String::from("Handshake"),
      Some(State::Http(h))      => h.print_state("HTTPS"),
      Some(State::WebSocket(_)) => String::from("WSS"),
      None                      => String::from("None"),
    };

    let r = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(ref expect, _)    => &expect.readiness,
      State::Handshake(ref handshake) => &handshake.readiness,
      State::Http(ref http)           => &http.front_readiness,
      State::WebSocket(ref pipe)      => &pipe.front_readiness,
    };

    error!("zombie session[{:?} => {:?}], state => readiness: {:?}, protocol: {}, app_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), r, p, self.app_id, self.back_connected, self.metrics);
  }

  fn tokens(&self) -> Vec<Token> {
    let mut v = vec![self.frontend_token];
    if let Some(tk) = self.back_token() {
      v.push(tk)
    }

    v
  }
}

fn version_str(version: ProtocolVersion) -> &'static str {
  match version {
    ProtocolVersion::SSLv2 => "tls.version.SSLv2",
    ProtocolVersion::SSLv3 => "tls.version.SSLv3",
    ProtocolVersion::TLSv1_0 => "tls.version.TLSv1_0",
    ProtocolVersion::TLSv1_1 => "tls.version.TLSv1_1",
    ProtocolVersion::TLSv1_2 => "tls.version.TLSv1_2",
    ProtocolVersion::TLSv1_3 => "tls.version.TLSv1_3",
    ProtocolVersion::Unknown(_) => "tls.version.Unknown",
  }
}

fn ciphersuite_str(cipher: &'static SupportedCipherSuite) -> &'static str {
  match cipher.suite {
    CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => "tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => "tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS13_CHACHA20_POLY1305_SHA256",
    CipherSuite::TLS13_AES_256_GCM_SHA384 => "tls.cipher.TLS13_AES_256_GCM_SHA384",
    CipherSuite::TLS13_AES_128_GCM_SHA256 => "tls.cipher.TLS13_AES_128_GCM_SHA256",
    _ => "tls.cipher.Unsupported",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /*
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    // fails depending on the platform?
    //assert_size!(Session, 2488);
    assert_size!(ExpectProxyProtocol<TcpStream>, 520);
    assert_size!(TlsHandshake, 1488);
    assert_size!(Http<FrontRustls>, 2456);
    assert_size!(Pipe<FrontRustls>, 1664);
    assert_size!(State, 2464);

    assert_size!(FrontRustls, 1456);
    assert_size!(ServerSession, 1440);
  }
  */
}
