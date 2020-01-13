use std::io::{self,ErrorKind,Read,Write};
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use mio::tcp::{TcpListener,TcpStream};
use rustls::{ServerSession, Session, ProtocolVersion};
use net2::TcpBuilder;
use net2::unix::UnixTcpBuilderExt;
#[cfg(feature = "use-openssl")]
use openssl::ssl::{ErrorCode, SslStream, SslVersion};

#[derive(Debug,PartialEq,Copy,Clone)]
pub enum SocketResult {
  Continue,
  Closed,
  WouldBlock,
  Error
}

#[derive(Debug,PartialEq,Copy,Clone)]
pub enum TransportProtocol {
  Tcp,
  Ssl2,
  Ssl3,
  Tls1_0,
  Tls1_1,
  Tls1_2,
  Tls1_3,
}

pub trait SocketHandler {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult);
  fn socket_write(&mut self, buf: &[u8])    -> (usize, SocketResult);
  fn socket_ref(&self) -> &TcpStream;
  fn protocol(&self) -> TransportProtocol;
}

impl SocketHandler for TcpStream {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.read(&mut buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size +=sz,
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            incr!("tcp.read.error");
            return (size, SocketResult::Closed)
          },
          _ => {
            error!("SOCKET\tsocket_read error={:?}", e);
            incr!("tcp.read.error");
            return (size, SocketResult::Error)
          },
        }
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.write(&buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size += sz,
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            incr!("tcp.write.error");
            return (size, SocketResult::Closed)
          },
          _ => {
            //FIXME: timeout and other common errors should be sent up
            error!("SOCKET\tsocket_write error={:?}", e);
            incr!("tcp.write.error");
            return (size, SocketResult::Error)
          },
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self }

  fn protocol(&self) -> TransportProtocol {
    TransportProtocol::Tcp
  }
}

#[cfg(feature = "use-openssl")]
impl SocketHandler for SslStream<TcpStream> {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.ssl_read(&mut buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size += sz,
        Err(e) => {
          match e.code() {
            ErrorCode::WANT_READ  => return (size, SocketResult::WouldBlock),
            ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
            ErrorCode::SSL        => {
              debug!("SOCKET-TLS\treadable TLS socket SSL error: {:?} -> {:?}", e, e.ssl_error());
              incr!("openssl.read.error");
              return (size, SocketResult::Error)
            },
            ErrorCode::SYSCALL    => {
              incr!("openssl.read.error");
              return (size, SocketResult::Error)
            },
            ErrorCode::ZERO_RETURN => {
              incr!("openssl.read.error");
              return (size, SocketResult::Closed)
            },
            _ => {
              debug!("SOCKET-TLS\treadable TLS socket error={:?} -> {:?}", e, e.ssl_error());
              incr!("openssl.read.error");
              return (size, SocketResult::Error)
            }
          }
        }
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.ssl_write(&buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size +=sz,
        Err(e) => {
          match e.code() {
            ErrorCode::WANT_READ  => return (size, SocketResult::WouldBlock),
            ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
            ErrorCode::SSL        => {
              debug!("SOCKET-TLS\twritable TLS socket SSL error: {:?} -> {:?}", e, e.ssl_error());
              incr!("openssl.write.error");
              return (size, SocketResult::Error)
            },
            ErrorCode::SYSCALL        => {
              debug!("SOCKET-TLS\twritable TLS socket syscall error: {:?} -> {:?}", e, e.ssl_error());
              incr!("openssl.write.error");
              return (size, SocketResult::Error)
            },
            ErrorCode::ZERO_RETURN => {
              incr!("openssl.write.error");
              return (size, SocketResult::Closed)
            },
            _ => {
              debug!("SOCKET-TLS\twritable TLS socket error={:?} -> {:?}", e, e.ssl_error());
              incr!("openssl.write.error");
              return (size, SocketResult::Error)
            }
          }
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }

  fn protocol(&self) -> TransportProtocol {
    self.ssl().version2().map(|version| match version {
      SslVersion::SSL3 => TransportProtocol::Ssl3,
      SslVersion::TLS1 => TransportProtocol::Tls1_0,
      SslVersion::TLS1_1 => TransportProtocol::Tls1_1,
      SslVersion::TLS1_2 => TransportProtocol::Tls1_2,
      _ => TransportProtocol::Tls1_3,
    }).unwrap_or(TransportProtocol::Tcp)
  }
}

pub struct FrontRustls {
  pub stream:  TcpStream,
  pub session: ServerSession,
}

impl SocketHandler for FrontRustls {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size      = 0usize;
    let mut can_read  = true;
    let mut is_error  = false;
    let mut is_closed = false;

    loop {
      if size == buf.len() {
        break;
      }

      if !can_read | is_error | is_closed {
        break;
      }

      match self.session.read_tls(&mut self.stream) {
        Ok(0) => {
          can_read  = false;
          is_closed = true;
        },
        Ok(_sz) => {},
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            can_read = false;
          },
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            incr!("rustls.read.error");
            is_closed = true;
          },
          _ => {
            error!("could not read TLS stream from socket: {:?}", e);
            incr!("rustls.read.error");
            is_error = true;
            break;
           }
        }
      }

      if let Err(e) = self.session.process_new_packets() {
        error!("could not process read TLS packets: {:?}", e);
        is_error = true;
        break;
      }

      while !self.session.wants_read() {
        match self.session.read(&mut buf[size..]) {
          Ok(sz) => size += sz,
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
              incr!("rustls.read.error");
              is_closed = true;
              break;
            },
            _ => {
              error!("could not read data from TLS stream: {:?}", e);
              incr!("rustls.read.error");
              is_error = true;
              break;
            }
          }
        }
      }
    }

    if is_error {
      (size, SocketResult::Error)
    } else if is_closed {
      (size, SocketResult::Closed)
    } else if !can_read {
      (size, SocketResult::WouldBlock)
    } else {
      (size, SocketResult::Continue)
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut buffered_size = 0usize;
    let mut _sent_size    = 0usize;
    let mut can_write     = true;
    let mut is_error      = false;
    let mut is_closed     = false;

    loop {
      if buffered_size == buf.len() {
        break;
      }

      if !can_write | is_error | is_closed {
        break;
      }

      match self.session.write(&buf[buffered_size..]) {
        Ok(0)  => {
          break;
        },
        Ok(sz) => {
          buffered_size += sz;
        },
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            // we don't need to do anything, the session will return false in wants_write?
            //error!("rustls socket_write wouldblock");
          },
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            //FIXME: this should probably not happen here
            incr!("rustls.write.error");
            is_closed = true;
            break;
          },
          _ => {
            error!("could not write data to TLS stream: {:?}", e);
            incr!("rustls.write.error");
            is_error = true;
            break;
          }
        }
      }

      loop {
        match self.session.write_tls(&mut self.stream) {
          Ok(0)  => {
            //can_write = false;
            break;
          },
          Ok(sz) => {
            _sent_size += sz;
          },
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => can_write = false,
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
              incr!("rustls.write.error");
              is_closed = true;
              break;
            },
            _ => {
              error!("could not write TLS stream to socket: {:?}", e);
              incr!("rustls.write.error");
              is_error = true;
              break;
            }
          }
        }
      }
    }

    if is_error {
      (buffered_size, SocketResult::Error)
    } else if is_closed {
      (buffered_size, SocketResult::Closed)
    } else if !can_write {
      (buffered_size, SocketResult::WouldBlock)
    } else {
      (buffered_size, SocketResult::Continue)
    }
  }

  fn socket_ref(&self) -> &TcpStream { &self.stream }

  fn protocol(&self) -> TransportProtocol {
    self.session.get_protocol_version().map(|version| match version {
      ProtocolVersion::SSLv2 => TransportProtocol::Ssl2,
      ProtocolVersion::SSLv3 => TransportProtocol::Ssl3,
      ProtocolVersion::TLSv1_0 => TransportProtocol::Tls1_0,
      ProtocolVersion::TLSv1_1 => TransportProtocol::Tls1_1,
      ProtocolVersion::TLSv1_2 => TransportProtocol::Tls1_2,
      ProtocolVersion::TLSv1_3 => TransportProtocol::Tls1_3,
      _ => TransportProtocol::Tls1_3,
    }).unwrap_or(TransportProtocol::Tcp)
  }
}

pub fn server_bind(addr: &SocketAddr) -> io::Result<TcpListener> {
  let sock = match *addr {
    SocketAddr::V4(..) => TcpBuilder::new_v4()?,
    SocketAddr::V6(..) => TcpBuilder::new_v6()?,
  };

  // set so_reuseaddr, but only on unix (mirrors what libstd does)
  if cfg!(unix) {
    sock.reuse_address(true)?;
  }

  sock.reuse_port(true)?;

  // bind the socket
  sock.bind(addr)?;

  // listen
  // FIXME: make the backlog configurable?
  let listener = sock.listen(1024)?;
  TcpListener::from_std(listener)
}


pub fn server_unbind(listener: &TcpListener) -> io::Result<()> {
  match unsafe { libc::close(listener.as_raw_fd()) } {
    0 => Ok(()),
    _ => Err(io::Error::last_os_error())
  }
}
