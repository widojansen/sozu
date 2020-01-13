use sozu_command::buffer::Buffer;
use buffer_queue::BufferQueue;
use protocol::http::StickySession;
use super::cookies::{RequestCookie, parse_request_cookies};
use features::FEATURES;

use nom::{
  HexDisplay,IResult,Offset,AsChar,Err,Needed,
  error::ErrorKind,
  character::{
    is_alphanumeric, is_space,
    streaming::{char, one_of},
    complete::digit1 as digit_complete
  },
  bytes::{
    self,
    streaming::{tag, take, take_while, take_while1},
    complete::{take_while1 as take_while1_complete}
  },
  sequence::{preceded, terminated, tuple},
  combinator::{opt, map_res}
};

use url::Url;

use std::{fmt,str};
use std::str::from_utf8;
use std::convert::From;
use std::collections::HashSet;

#[cfg(test)]
mod tests;

pub fn compare_no_case(left: &[u8], right: &[u8]) -> bool {
  if left.len() != right.len() {
    return false;
  }

  left.iter().zip(right).all(|(a, b)| match (*a, *b) {
    (0..=64, 0..=64) | (91..=96, 91..=96) | (123..=255, 123..=255) => a == b,
    (65..=90, 65..=90) | (97..=122, 97..=122) | (65..=90, 97..=122) | (97..=122, 65..=90) => *a | 0b00_10_00_00 == *b | 0b00_10_00_00,
    _ => false
  })
}

// Primitives
fn is_token_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  b"!#$%&'*+-.^_`|~".contains(&i)
}

fn token(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_token_char)(i)
}

fn is_status_token_char(i: u8) -> bool {
  i >= 32 && i != 127
}

fn status_token(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_status_token_char)(i)
}

fn is_ws(i: u8) -> bool {
  i == b' ' && i == b'\t'
}

fn sp(i:&[u8]) -> IResult<&[u8], char> {
  char(' ')(i)
}

fn crlf(i:&[u8]) -> IResult<&[u8], &[u8]> {
  tag("\r\n")(i)
}

fn is_vchar(i: u8) -> bool {
  i > 32 && i <= 126
}
fn is_vchar_or_ws(i: u8) -> bool {
  is_vchar(i) || is_ws(i)
}

// allows ISO-8859-1 characters in header values
// this is allowed in RFC 2616 but not in rfc7230
// cf https://github.com/sozu-proxy/sozu/issues/479
#[cfg(feature = "tolerant-http1-parser")]
fn is_header_value_char(i: u8) -> bool {
  i == 9 || (i >= 32 && i <= 126) || i >= 160
}

#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_header_value_char(i: u8) -> bool {
  i == 9 || (i >= 32 && i <= 126)
}

fn vchar_1(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_vchar)(i)
}

fn vchar_ws_1(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_vchar_or_ws)(i)
}

#[derive(PartialEq,Debug,Clone)]
pub enum Method {
  Get,
  Post,
  Head,
  Options,
  Put,
  Delete,
  Trace,
  Connect,
  Custom(String),
}

impl Method {
  pub fn new(s: &[u8]) -> Method {
    if compare_no_case(&s, b"GET") {
      Method::Get
    } else if compare_no_case(&s, b"POST") {
      Method::Post
    } else if compare_no_case(&s, b"HEAD") {
      Method::Head
    } else if compare_no_case(&s, b"OPTIONS") {
      Method::Options
    } else if compare_no_case(&s, b"PUT") {
      Method::Put
    } else if compare_no_case(&s, b"DELETE") {
      Method::Delete
    } else if compare_no_case(&s, b"TRACE") {
      Method::Trace
    } else if compare_no_case(&s, b"CONNECT") {
      Method::Connect
    } else {
      Method::Custom(String::from(unsafe { str::from_utf8_unchecked(s) }))
    }
  }
}

impl fmt::Display for Method {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
     Method::Get => write!(f, "GET"),
     Method::Post => write!(f, "POST"),
     Method::Head => write!(f, "HEAD"),
     Method::Options => write!(f, "OPTIONS"),
     Method::Put => write!(f, "PUT"),
     Method::Delete => write!(f, "DELETE"),
     Method::Trace => write!(f, "TRACE"),
     Method::Connect => write!(f, "CONNECT"),
     Method::Custom(s)  => write!(f, "{}", s),
    }
  }
}

#[derive(PartialEq,Debug,Clone,Copy)]
pub enum Version {
  V10,
  V11,
}

#[derive(PartialEq,Debug)]
pub struct RequestLine<'a> {
    pub method: &'a [u8],
    pub uri: &'a [u8],
    pub version: Version
}

#[derive(PartialEq,Debug,Clone)]
pub struct RRequestLine {
    pub method: Method,
    pub uri: String,
    pub version: Version
}

impl RRequestLine {
  pub fn from_request_line(r: RequestLine) -> Option<RRequestLine> {
    if let Ok(uri) = str::from_utf8(r.uri) {
      Some(RRequestLine {
        method:  Method::new(r.method),
        uri:     String::from(uri),
        version: r.version
      })
    } else {
      None
    }
  }
}

#[derive(PartialEq,Debug)]
pub struct StatusLine<'a> {
    pub version: Version,
    pub status: &'a [u8],
    pub reason: &'a [u8],
}

#[derive(PartialEq,Debug,Clone)]
pub struct RStatusLine {
    pub version: Version,
    pub status:  u16,
    pub reason:  String,
}

impl RStatusLine {
  pub fn from_status_line(r: StatusLine) -> Option<RStatusLine> {
    if let Ok(status_str) = str::from_utf8(r.status) {
      if let Ok(status) = status_str.parse::<u16>() {
        if let Ok(reason) = str::from_utf8(r.reason) {
          Some(RStatusLine {
            version: r.version,
            status,
            reason:  String::from(reason),
          })
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    }
  }
}

fn http_version(i:&[u8]) -> IResult<&[u8], Version> {
  let (i, _) = tag("HTTP/1.")(i)?;
  let (i, minor) = one_of("01")(i)?;

  Ok((i, if minor == '0' {
    Version::V10
  } else {
    Version::V11
  }))
}

fn request_line(i:&[u8]) -> IResult<&[u8], RequestLine> {
  let (i, method) = token(i)?;
  let (i, _) = sp(i)?;
  let (i, uri) = vchar_1(i)?; // ToDo proper URI parsing?
  let (i, _) = sp(i)?;
  let (i, version) = http_version(i)?;
  let (i, _) = crlf(i)?;

  Ok((i, RequestLine {
    method: method,
    uri: uri,
    version: version
  }))
}

fn status_line(i:&[u8]) -> IResult<&[u8], StatusLine> {
  let (i, (version, _, status, _, reason, _)) =
    tuple((http_version, sp, take(3usize), sp, status_token, crlf))(i)?;

  Ok((i, StatusLine {
    version: version,
    status: status,
    reason: reason,
  }))
}

#[derive(PartialEq,Debug)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8]
}

fn message_header(i:&[u8]) -> IResult<&[u8], Header> {
  // ToDo handle folding?
  let (i, (name, _, _, value, _)) =
    tuple((token, tag(":"), take_while(is_space), take_while(is_header_value_char), crlf))(i)?;

  Ok((i, Header {
    name: name,
    value: value
  }))
}

//not a space nor a comma
//
// allows ISO-8859-1 characters in header values
// this is allowed in RFC 2616 but not in rfc7230
// cf https://github.com/sozu-proxy/sozu/issues/479
#[cfg(feature = "tolerant-http1-parser")]
fn is_single_header_value_char(i: u8) -> bool {
  (i > 33 && i <= 126 && i != 44) || i >= 160
}

//not a space nor a comma
#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_single_header_value_char(i: u8) -> bool {
  i > 33 && i <= 126 && i != 44
}

fn single_header_value(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while1_complete(is_single_header_value_char)(i)
}

#[cfg(feature = "tolerant-http1-parser")]
fn is_hostname_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  // the domain name should not start with a hyphen or dot
  // but is it important here, since we will match this to
  // the list of accepted applications?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain shuld not be larger than 253 chars
  //
  // this tolerant parser also allows underscore, which is wrong
  // in domain names but accepted by some proxies and web servers
  // see https://github.com/sozu-proxy/sozu/issues/480
  b"-._".contains(&i)
}

#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_hostname_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  // the domain name should not start with a hyphen or dot
  // but is it important here, since we will match this to
  // the list of accepted applications?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain shuld not be larger than 253 chars
  b"-.".contains(&i)
}

// FIXME: convert port to u16 here
pub fn hostname_and_port(i: &[u8]) -> IResult<&[u8], (&[u8], Option<&[u8]>)> {
  let (i, host) = take_while1_complete(is_hostname_char)(i)?;
  let (i, port) = opt(preceded(bytes::complete::tag(":"), digit_complete))(i)?;

  if !i.is_empty() {
    Err(Err::Error((i, ErrorKind::Eof)))
  } else {
    Ok((i, (host, port)))
  }
}

pub fn is_hex_digit(chr: u8) -> bool {
  (chr >= 0x30 && chr <= 0x39) ||
  (chr >= 0x41 && chr <= 0x46) ||
  (chr >= 0x61 && chr <= 0x66)
}
pub fn chunk_size(input: &[u8]) -> IResult<&[u8], usize> {
  let (i, s) = map_res(take_while(is_hex_digit), from_utf8)(input)?;
  if i.is_empty() {
    return Err(Err::Incomplete(Needed::Unknown));
  }
  match usize::from_str_radix(s, 16) {
    Ok(sz) => Ok((i, sz)),
    Err(_) => Err(Err::Error(error_position!(input, ::nom::error::ErrorKind::MapRes)))
  }
}

fn chunk_header(i: &[u8]) -> IResult<&[u8], usize> {
  terminated(chunk_size, crlf)(i)
}

fn end_of_chunk_and_header(i: &[u8]) -> IResult<&[u8], usize> {
  preceded(crlf, chunk_header)(i)
}

fn trailer_line(i: &[u8]) -> IResult<&[u8], &[u8]> {
  terminated(take_while1(is_header_value_char), crlf)(i)
}

#[derive(PartialEq,Debug,Clone,Copy)]
pub enum Chunk {
  Initial,
  Copying,
  CopyingLastHeader,
  Ended,
  Error
}

impl Chunk {
  pub fn should_copy(&self) -> bool {
    Chunk::Copying == *self
  }

  pub fn should_parse(&self) -> bool {
    match *self {
      Chunk::Initial | Chunk::Copying | Chunk::CopyingLastHeader => true,
      _                                                          => false
    }
  }

  pub fn has_ended(&self) -> bool {
    *self == Chunk::Ended
  }

  pub fn is_error(&self) -> bool {
    *self == Chunk::Error
  }

  // FIXME: probably inefficient, since we don't parse again until the previous chunk was sent
  // it should be possible to parse the next header from a specific position like parse_*_until_stop
  // and return the biggest copying size
  pub fn parse_one(&self, buf: &[u8]) -> (usize, Chunk) {
    match *self {
      // we parse the first header, and advance the position to the end of chunk
      Chunk::Initial => {
        match chunk_header(buf) {
          Ok((i, sz)) => {
            if sz == 0 {
              // size of header + 0 data
              (buf.offset(i), Chunk::CopyingLastHeader)
            } else {
              // size of header + size of data
              (buf.offset(i) + sz, Chunk::Copying)
            }
          },
          Err(Err::Incomplete(_)) => (0, Chunk::Initial),
          Err(_)     => (0, Chunk::Error)
        }
      },
      // we parse a crlf then a header, and advance the position to the end of chunk
      Chunk::Copying => {
        match end_of_chunk_and_header(buf) {
          Ok((i, sz_str)) => {
            let sz = usize::from(sz_str);
            if sz == 0 {
              // data to copy + size of header + 0 data
              (buf.offset(i), Chunk::CopyingLastHeader)
            } else {
              // data to copy + size of header + size of next chunk
              (buf.offset(i)+sz, Chunk::Copying)
            }
          },
          Err(Err::Incomplete(_)) => (0, Chunk::Copying),
          Err(_) => (0, Chunk::Error)
        }
      },
      // we parse a crlf then stop
      Chunk::CopyingLastHeader => {
        match crlf(buf) {
          Ok((i, _)) => {
            (buf.offset(i), Chunk::Ended)
          },
          Err(Err::Incomplete(_)) => (0, Chunk::CopyingLastHeader),
          Err(_) => (0, Chunk::Error)
        }
      },
      _ => { (0, Chunk::Error) }
    }
  }

  //pub fn parse
  pub fn parse(&self, buf: &[u8]) -> (BufferMove, Chunk) {
    let mut current_state = *self;
    let mut position      = 0;
    let length            = buf.len();
    loop {
      let (mv, new_state) = current_state.parse_one(&buf[position..]);
      current_state = new_state;
      position += mv;
      if mv == 0 {
        break;
      }

      match current_state {
        Chunk::Ended | Chunk::Error => {
          break;
        },
        _ => {}
      }

      if position >= length {
        break;
      }

    }

    match position {
      0  => (BufferMove::None, current_state),
      sz => (BufferMove::Advance(sz), current_state)
    }
  }
}

#[derive(PartialEq,Debug)]
pub enum TransferEncodingValue {
  Chunked,
  Compress,
  Deflate,
  Gzip,
  Identity,
  Unknown
}

#[derive(PartialEq,Debug)]
pub struct ConnectionValue {
  pub has_close: bool,
  pub has_keep_alive: bool,
  pub has_upgrade: bool,
  pub to_delete: Option<HashSet<Vec<u8>>>,
}

#[derive(PartialEq,Debug)]
pub enum HeaderResult<T> {
  Value(T),
  None,
  Error
}

impl<'a> Header<'a> {
  pub fn value(&self) -> HeaderValue {
    if compare_no_case(self.name, b"host") {
      //FIXME: UTF8 conversion should be unchecked here, since we already checked the tokens?
      if let Some(s) = str::from_utf8(self.value).map(String::from).ok() {
        HeaderValue::Host(s)
      } else {
        HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"content-length") {
      if let Ok(l) = str::from_utf8(self.value) {
        if let Some(length) = l.parse().ok() {
           return HeaderValue::ContentLength(length)
        }
      }
      HeaderValue::Error
    } else if compare_no_case(self.name, b"transfer-encoding") {
      if compare_no_case(&self.value, b"chunked") {
        HeaderValue::Encoding(TransferEncodingValue::Chunked)
      } else if compare_no_case(&self.value, b"compress") {
        HeaderValue::Encoding(TransferEncodingValue::Compress)
      } else if compare_no_case(&self.value, b"deflate") {
        HeaderValue::Encoding(TransferEncodingValue::Deflate)
      } else if compare_no_case(&self.value, b"gzip") {
        HeaderValue::Encoding(TransferEncodingValue::Gzip)
      } else if compare_no_case(&self.value, b"identity") {
        HeaderValue::Encoding(TransferEncodingValue::Identity)
      } else {
        HeaderValue::Encoding(TransferEncodingValue::Unknown)
      }
    } else if compare_no_case(self.name, b"connection") {
      let mut has_close = false;
      let mut has_upgrade = false;
      let mut has_keep_alive = false;
      let mut to_delete = None;

      match single_header_value(self.value) {
        Ok((mut input, first)) => {
          if compare_no_case(first, b"upgrade") {
            has_upgrade = true;
          } else if compare_no_case(first, b"close") {
            has_close = true;
          } else if compare_no_case(first, b"keep-alive") {
            has_keep_alive = true;
          } else {
            if to_delete.is_none() {
              to_delete = Some(HashSet::new());
            }

            to_delete.as_mut().map(|h| h.insert(Vec::from(first)));
          }

          while input.len() != 0 {
            match do_parse!(input,
              opt!(complete!(sp)) >>
              complete!(char!(',')) >>
              opt!(sp) >>
              v: single_header_value >> (v)
            ) {
              Ok((i, v)) => {
                if compare_no_case(v, b"upgrade") {
                  has_upgrade = true;
                } else if compare_no_case(v, b"close") {
                  has_close = true;
                } else if compare_no_case(v, b"keep-alive") {
                  has_keep_alive = true;
                } else {
                  if to_delete.is_none() {
                    to_delete = Some(HashSet::new());
                  }

                  to_delete.as_mut().map(|h| h.insert(Vec::from(v)));
                }

                input = i;
              },
              Err(_) => {
                return HeaderValue::Error;
              }
            }
          }
          let r = ConnectionValue {
            has_close, has_keep_alive, has_upgrade, to_delete
          };
          //println!("returning: {:?}", r);
          HeaderValue::Connection(r)
        },
        Err(_) => HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"upgrade") {
      HeaderValue::Upgrade(self.value)
    } else if compare_no_case(self.name, b"forwarded")   ||
        compare_no_case(self.name, b"x-forwarded-for")   ||
        compare_no_case(self.name, b"x-forwarded-proto") ||
        compare_no_case(self.name, b"x-forwarded-port") {
      HeaderValue::Forwarded
    } else if compare_no_case(self.name, b"expect") {
      if compare_no_case(self.value, b"100-continue") {
        HeaderValue::ExpectContinue
      } else {
        HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"cookie") {
      match parse_request_cookies(self.value) {
        Some(cookies) => HeaderValue::Cookie(cookies),
        None          => HeaderValue::Error
      }
    } else {
      HeaderValue::Other(self.name, self.value)
    }
  }

  pub fn should_delete(&self, conn: &Connection, sticky_name: &str) -> bool {
    //FIXME: we should delete this header anyway, and add a Connection: Upgrade if we detected an upgrade
    if compare_no_case(&self.name, b"connection") {
      match single_header_value(self.value) {
        Ok((mut input, first)) => {
          if compare_no_case(first, b"upgrade") {
            false
          } else {
            while input.len() != 0 {
              match do_parse!(input,
                opt!(complete!(sp)) >>
                complete!(char!(',')) >>
                opt!(sp) >>
                v: single_header_value >> (v)
              ) {
                Ok((i, v)) => {
                  if compare_no_case(v, b"upgrade") {
                    return false;
                  }
                  input = i;
                },
                Err(_) => {
                  return true;
                }
              }
            }
            true

          }
        },
        Err(_) => true
      }
    } else if compare_no_case(&self.name, b"set-cookie") {
      self.value.starts_with(sticky_name.as_bytes())
    } else {
      let mut b = (compare_no_case(&self.name, b"connection") && !compare_no_case(&self.value, b"upgrade")) ||
      compare_no_case(&self.name, b"sozu-id")           ||
      {
        let mut res = false;
        if let Some(ref to_delete) = conn.to_delete {
          for ref header_value in to_delete {
            if compare_no_case(&self.value, &header_value) {
              res = true;
              break;
            }
          }
        }

        res
      };

      if !FEATURES.with(|features| features.borrow().get("forwarded-fix").map(|f| f.is_true()).unwrap_or(false)) {
        b |= compare_no_case(&self.name, b"forwarded")         ||
             compare_no_case(&self.name, b"x-forwarded-for")   ||
             compare_no_case(&self.name, b"x-forwarded-proto") ||
             compare_no_case(&self.name, b"x-forwarded-port");
      }

      b
    }
  }

  pub fn must_mutate(&self) -> bool {
    compare_no_case(&self.name, b"cookie")
  }

  pub fn mutate_header(&self, buf: &[u8], offset: usize, sticky_name: &str) -> Vec<BufferMove> {
    if compare_no_case(&self.name, b"cookie") {
      self.remove_sticky_cookie_in_request(buf, offset, sticky_name)
    } else {
      vec![BufferMove::Advance(offset)]
    }
  }

  pub fn remove_sticky_cookie_in_request(&self, buf: &[u8], offset: usize, sticky_name: &str) -> Vec<BufferMove> {
    if let Some(cookies) = parse_request_cookies(self.value) {
      // if we don't find the cookie, don't go further
      if let Some(sozu_balance_position) = cookies.iter().position(|cookie| &cookie.name[..] == sticky_name.as_bytes()) {
        // If we have only one cookie and that's the one, then we drop the whole header
        if cookies.len() == 1 {
          return vec![BufferMove::Delete(offset)];
        }
        // we want to advance the buffer for the header's name
        // +1 is to count ":"
        let header_length = self.name.len() + 1;

        fn take_space(i: &[u8]) -> IResult<&[u8], &[u8]> {
          take_while(is_space)(i)
        }
        // we calculate how much chars there is between the : and the first cookie
        let length_until_value = match take_space(&buf[header_length..buf.len()]) {
          Ok((_, spaces)) => spaces,
          Err(_) => {
            // if there is not enough data or an error, we completely remove the header.
            return vec![BufferMove::Advance(offset)];
          }
        };

        // Our iterator over the cookies
        let mut iter = cookies.iter();
        // Our return value
        let mut moves = Vec::new();
        // The current number of cookie parsed
        let mut current_cookie = 0;
        // If the cookie SOZUBALANCEID is the last of the cookie chain
        let sozu_balance_is_last = (sozu_balance_position + 1) == cookies.len();

        moves.push(BufferMove::Advance(header_length + length_until_value.len()));

        loop {
          match iter.next() {
            Some(cookie) => {
              let cookie_length = cookie.get_full_length();
              // We already know the position of the cookie in the chain, so we avoid
              // a string comparision and directly check against where we are in the cookies
              if current_cookie == sozu_balance_position {
                moves.push(BufferMove::Delete(cookie_length));
              } else if sozu_balance_is_last {
                // if sozublanceid is the last element, we want to delete the "; " chars from
                // the before last cookie
                if (current_cookie + 1) == sozu_balance_position {
                  let spaces = cookie.spaces.len();
                  // This one is obvious but I prefer to name the value
                  let semicolon = 1;
                  moves.push(BufferMove::Advance(cookie_length - spaces - semicolon));
                  // We directly do the Delete here to avoid keeping context of 'did the cookie
                  // before had spaces ?'
                  moves.push(BufferMove::Delete(semicolon + spaces));
                } else {
                  moves.push(BufferMove::Advance(cookie_length));
                }
              } else {
                moves.push(BufferMove::Advance(cookie_length));
              }

              current_cookie += 1;
            },
            None => {
              moves.push(BufferMove::Advance(2)); // advance of 2 for the header's \r\n
              return moves;
            }
          }
        }
      }
    }

    vec![BufferMove::Advance(offset)]
  }
}

pub enum ForwardedProtocol {
  HTTP,
  HTTPS
}

pub enum HeaderValue<'a> {
  Host(String),
  ContentLength(usize),
  Encoding(TransferEncodingValue),
  //FIXME: are the references in Connection still valid after we delete that part of the headers?
  Connection(ConnectionValue),
  Upgrade(&'a[u8]),
  Cookie(Vec<RequestCookie<'a>>),
  Other(&'a[u8],&'a[u8]),
  Forwarded,
  ExpectContinue,
  /*
  Forwarded(Vec<&'a[u8]>),
  XForwardedFor(Vec<&'a[u8]>),
  XForwardedProto(ForwardedProtocol),
  XForwardedPort(u16),
  */
  Error
}

pub type Host = String;

#[derive(Debug,Clone,PartialEq)]
pub enum LengthInformation {
  Length(usize),
  Chunked,
  //Compressed
}

#[derive(Debug,Clone,PartialEq)]
pub enum Continue {
  None,
  Expects(usize),
}
/*
#[derive(Debug,Clone,PartialEq)]
pub enum Connection {
  KeepAlive,
  Close,
  Upgrade,
}
*/


#[derive(Debug,Clone,PartialEq)]
pub struct Connection {
  pub keep_alive:     Option<bool>,
  pub has_upgrade:    bool,
  pub upgrade:        Option<String>,
  pub to_delete:      Option<HashSet<Vec<u8>>>,
  pub continues:      Continue,
  pub sticky_session: Option<String>,
}

impl Connection {
  pub fn new() -> Connection {
    Connection {
      keep_alive:     None,
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None,
    }
  }

  pub fn keep_alive() -> Connection {
    Connection {
      keep_alive:     Some(true),
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None,
    }
  }

  pub fn close() -> Connection {
    Connection {
      keep_alive:     Some(false),
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None
    }
  }
}

#[derive(Debug,Clone,PartialEq)]
pub enum RequestState {
  Initial,
  Error(Option<RRequestLine>, Option<Connection>, Option<Host>, Option<LengthInformation>, Option<Chunk>),
  HasRequestLine(RRequestLine, Connection),
  HasHost(RRequestLine, Connection, Host),
  HasLength(RRequestLine, Connection, LengthInformation),
  HasHostAndLength(RRequestLine, Connection, Host, LengthInformation),
  Request(RRequestLine, Connection, Host),
  RequestWithBody(RRequestLine, Connection, Host, usize),
  RequestWithBodyChunks(RRequestLine, Connection, Host, Chunk),
}

impl RequestState {
  pub fn into_error(self) -> RequestState {
    match self {
      RequestState::Initial => RequestState::Error(None, None, None, None, None),
      RequestState::HasRequestLine(rl, conn) => RequestState::Error(Some(rl), Some(conn), None, None, None),
      RequestState::HasHost(rl, conn, host)  => RequestState::Error(Some(rl), Some(conn), Some(host), None, None),
      RequestState::HasHostAndLength(rl, conn, host, len)  => RequestState::Error(Some(rl), Some(conn), Some(host), Some(len), None),
      RequestState::Request(rl, conn, host)  => RequestState::Error(Some(rl), Some(conn), Some(host), None, None),
      RequestState::RequestWithBody(rl, conn, host, len) => RequestState::Error(Some(rl), Some(conn), Some(host), Some(LengthInformation::Length(len)), None),
      RequestState::RequestWithBodyChunks(rl, conn, host, chunk) => RequestState::Error(Some(rl), Some(conn), Some(host), None, Some(chunk)),
      err => err,
    }
  }

  pub fn is_front_error(&self) -> bool {
    if let RequestState::Error(_,_,_,_,_) = self {
      true
    } else {
      false
    }
  }

  pub fn get_sticky_session(&self) -> Option<String> {
    self.get_keep_alive().and_then(|con| con.sticky_session)
  }

  pub fn has_host(&self) -> bool {
    match *self {
      RequestState::HasHost(_, _, _)            |
      RequestState::HasHostAndLength(_, _, _, _)|
      RequestState::Request(_, _, _)            |
      RequestState::RequestWithBody(_, _, _, _) |
      RequestState::RequestWithBodyChunks(_, _, _, _) => true,
      _                                               => false
    }
  }

  pub fn is_proxying(&self) -> bool {
    match *self {
      RequestState::Request(_, _, _)            |
      RequestState::RequestWithBody(_, _, _, _) |
      RequestState::RequestWithBodyChunks(_, _, _, _)  => true,
      _                                                => false
    }
  }

  pub fn is_head(&self) -> bool {
    match *self {
      RequestState::Request(ref rl, _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => {
        rl.method == Method::Head
      },
      _                                                => false
    }
  }

  pub fn get_host(&self) -> Option<String> {
    match *self {
      RequestState::HasHost(_, _, ref host)            |
      RequestState::Request(_, _, ref host)            |
      RequestState::RequestWithBody(_, _, ref host, _) |
      RequestState::RequestWithBodyChunks(_, _, ref host, _) => Some(host.clone()),
      RequestState::Error(_, _, ref host, _, _)              => host.clone(),
      _                                                      => None
    }
  }

  pub fn get_uri(&self) -> Option<String> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)        |
      RequestState::HasHost(ref rl, _, _)            |
      RequestState::Request(ref rl , _, _)           |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => Some(rl.uri.clone()),
      RequestState::Error(ref rl, _, _, _, _)              => rl.as_ref().map(|r| r.uri.clone()),
      _                                                    => None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)        |
      RequestState::HasHost(ref rl, _, _)            |
      RequestState::Request(ref rl, _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => Some(rl.clone()),
      RequestState::Error(ref rl, _, _, _, _)              => rl.clone(),
      _                                                    => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<Connection> {
    match *self {
      RequestState::HasRequestLine(_, ref conn)         |
      RequestState::HasHost(_, ref conn, _)             |
      RequestState::HasLength(_, ref conn, _)           |
      RequestState::HasHostAndLength(_, ref conn, _, _) |
      RequestState::Request(_, ref conn, _)             |
      RequestState::RequestWithBody(_, ref conn, _, _)  |
      RequestState::RequestWithBodyChunks(_, ref conn, _, _) => Some(conn.clone()),
      RequestState::Error(_, ref conn, _, _, _)       => conn.clone(),
      _                                                      => None
    }
  }

  pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
    match *self {
      RequestState::HasRequestLine(_, ref mut conn)         |
      RequestState::HasHost(_, ref mut conn, _)             |
      RequestState::HasLength(_, ref mut conn, _)           |
      RequestState::HasHostAndLength(_, ref mut conn, _, _) |
      RequestState::Request(_, ref mut conn, _)             |
      RequestState::RequestWithBody(_, ref mut conn, _, _)  |
      RequestState::RequestWithBodyChunks(_, ref mut conn, _, _) => Some(conn),
      _                                                      => None
    }

  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      RequestState::RequestWithBody(_, _, _, l) => Some(position + l),
      RequestState::Request(_, _, _)            => Some(position),
      _                                         => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    //FIXME: should not clone here
    let rl =  self.get_request_line();
    let version = rl.as_ref().map(|rl| rl.version);
    let conn = self.get_keep_alive();
    match (version, conn.map(|c| c.keep_alive)) {
      (_, Some(Some(true)))   => true,
      (_, Some(Some(false)))  => false,
      (Some(Version::V10), _) => false,
      (Some(Version::V11), _) => true,
      (_, _)                  => false,
    }
  }

  pub fn should_chunk(&self) -> bool {
    if let  RequestState::RequestWithBodyChunks(_, _, _, _) = *self {
      true
    } else {
      false
    }
  }
}

pub type UpgradeProtocol = String;

#[derive(Debug,Clone,PartialEq)]
pub enum ResponseState {
  Initial,
  Error(Option<RStatusLine>, Option<Connection>, Option<UpgradeProtocol>, Option<LengthInformation>, Option<Chunk>),
  HasStatusLine(RStatusLine, Connection),
  HasUpgrade(RStatusLine, Connection, UpgradeProtocol),
  HasLength(RStatusLine, Connection, LengthInformation),
  Response(RStatusLine, Connection),
  ResponseUpgrade(RStatusLine, Connection, UpgradeProtocol),
  ResponseWithBody(RStatusLine, Connection, usize),
  ResponseWithBodyChunks(RStatusLine, Connection, Chunk),
  // the boolean indicates if the backend connection is closed
  ResponseWithBodyCloseDelimited(RStatusLine, Connection, bool),
}

impl ResponseState {
  pub fn into_error(self) -> ResponseState {
    match self {
      ResponseState::Initial => ResponseState::Error(None, None, None, None, None),
      ResponseState::HasStatusLine(sl, conn) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      ResponseState::HasLength(sl, conn, length) => ResponseState::Error(Some(sl), Some(conn), None, Some(length), None),
      ResponseState::HasUpgrade(sl, conn, upgrade) => ResponseState::Error(Some(sl), Some(conn), Some(upgrade), None, None),
      ResponseState::Response(sl, conn) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      ResponseState::ResponseUpgrade(sl, conn, upgrade) => ResponseState::Error(Some(sl), Some(conn), Some(upgrade), None, None),
      ResponseState::ResponseWithBody(sl, conn, len) => ResponseState::Error(Some(sl), Some(conn), None, Some(LengthInformation::Length(len)), None),
      ResponseState::ResponseWithBodyChunks(sl, conn, chunk) => ResponseState::Error(Some(sl), Some(conn), None, None, Some(chunk)),
      ResponseState::ResponseWithBodyCloseDelimited(sl, conn, _) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      err => err
    }
  }

  pub fn is_proxying(&self) -> bool {
    match *self {
        ResponseState::Response(_, _)
      | ResponseState::ResponseWithBody(_, _, _)
      | ResponseState::ResponseWithBodyChunks(_, _, _)
      | ResponseState::ResponseWithBodyCloseDelimited(_, _, _)
        => true,
      _ => false
    }
  }

  pub fn is_back_error(&self) -> bool {
    if let ResponseState::Error(_,_,_,_,_) = self {
      true
    } else {
      false
    }
  }

  pub fn get_status_line(&self) -> Option<RStatusLine> {
    match *self {
      ResponseState::HasStatusLine(ref sl, _)             |
      ResponseState::HasLength(ref sl, _, _)              |
      ResponseState::HasUpgrade(ref sl, _, _)             |
      ResponseState::Response(ref sl, _)                  |
      ResponseState::ResponseUpgrade(ref sl, _, _)        |
      ResponseState::ResponseWithBody(ref sl, _, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(ref sl, _, _) |
      ResponseState::ResponseWithBodyChunks(ref sl, _, _) => Some(sl.clone()),
      ResponseState::Error(ref sl, _, _, _, _)            => sl.clone(),
      _                                                   => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<Connection> {
    match *self {
      ResponseState::HasStatusLine(_, ref conn)             |
      ResponseState::HasLength(_, ref conn, _)              |
      ResponseState::HasUpgrade(_, ref conn, _)             |
      ResponseState::Response(_, ref conn)                  |
      ResponseState::ResponseUpgrade(_, ref conn, _)        |
      ResponseState::ResponseWithBody(_, ref conn, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(_, ref conn, _) |
      ResponseState::ResponseWithBodyChunks(_, ref conn, _) => Some(conn.clone()),
      ResponseState::Error(_, ref conn, _, _, _)            => conn.clone(),
      _                                                     => None
    }
  }

  pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
    match *self {
      ResponseState::HasStatusLine(_, ref mut conn)             |
      ResponseState::HasLength(_, ref mut conn, _)              |
      ResponseState::HasUpgrade(_, ref mut conn, _)             |
      ResponseState::Response(_, ref mut conn)                  |
      ResponseState::ResponseUpgrade(_, ref mut conn, _)        |
      ResponseState::ResponseWithBody(_, ref mut conn, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(_, ref mut conn, _) |
      ResponseState::ResponseWithBodyChunks(_, ref mut conn, _) => Some(conn),
      ResponseState::Error(_, ref mut conn, _, _, _)            => conn.as_mut(),
      _                                                     => None
    }
  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      ResponseState::ResponseWithBody(_, _, l) => Some(position + l),
      ResponseState::Response(_, _)            => Some(position),
      _                                        => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    //FIXME: should not clone here
    let sl      = self.get_status_line();
    let version = sl.as_ref().map(|sl| sl.version);
    let conn    = self.get_keep_alive();
    match (version, conn.map(|c| c.keep_alive)) {
      (_, Some(Some(true)))   => true,
      (_, Some(Some(false)))  => false,
      (Some(Version::V10), _) => false,
      (Some(Version::V11), _) => true,
      (_, _)                  => false,
    }
  }

  pub fn should_chunk(&self) -> bool {
    if let  ResponseState::ResponseWithBodyChunks(_, _, _) = *self {
      true
    } else {
      false
    }
  }
}

pub type HeaderEndPosition = Option<usize>;

#[derive(Debug,PartialEq)]
pub enum BufferMove {
  None,
  /// length
  Advance(usize),
  /// length
  Delete(usize),
  /// Vec of BufferMove operations
  Multiple(Vec<BufferMove>)
}

pub fn default_request_result<O>(state: RequestState, res: IResult<&[u8], O>) -> (BufferMove, RequestState) {
  match res {
    Err(Err::Error(_)) | Err(Err::Failure(_)) => (BufferMove::None, state.into_error()),
    Err(Err::Incomplete(_)) => (BufferMove::None, state),
    _                      => unreachable!()
  }
}

pub fn validate_request_header(mut state: RequestState, header: &Header, sticky_name: &str) -> RequestState {
  match header.value() {
    HeaderValue::Host(host) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasHost(rl, conn, host),
        RequestState::HasLength(rl, conn, l)   => RequestState::HasHostAndLength(rl, conn, host, l),
        s                                      => s.into_error()
      }
    },
    HeaderValue::ContentLength(sz) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasLength(rl, conn, LengthInformation::Length(sz)),
        RequestState::HasHost(rl, conn, host)  => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Length(sz)),
        s                                      => s.into_error()
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        RequestState::HasRequestLine(rl, conn)            => RequestState::HasLength(rl, conn, LengthInformation::Chunked),
        RequestState::HasHost(rl, conn, host)             => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        // Transfer-Encoding takes the precedence on Content-Length
        RequestState::HasHostAndLength(rl, conn, host,
           LengthInformation::Length(_))         => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        s                                        => s.into_error()
      }
    },
    // FIXME: for now, we don't remember if we cancel indications from a previous Connection Header
    HeaderValue::Connection(c) => {
      if state.get_mut_connection().map(|conn| {
        if c.has_close {
          conn.keep_alive = Some(false);
        }
        if c.has_keep_alive {
          conn.keep_alive = Some(true);
        }
        if c.has_upgrade {
          conn.has_upgrade = true;
        }
      }).is_some() {
        state
      } else {
        state.into_error()
      }
    },
    HeaderValue::ExpectContinue => {
      if state.get_mut_connection().map(|conn| {
        conn.continues = Continue::Expects(0);
      }).is_some() {
        state
      } else {
        state.into_error()
      }
    }

    /*
    HeaderValue::Forwarded(_)  => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedFor(_) => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedProto(_) => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedPort(_) => RequestState::Error(ErrorState::InvalidHttp),
    */
    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => state.into_error(),
    HeaderValue::Forwarded   => state,
    HeaderValue::Other(_,_)  => state,
    //FIXME: for now, we don't look at what is asked in upgrade since the backend is the one deciding
    HeaderValue::Upgrade(s)  => {
      let mut st = state;
      st.get_mut_connection().map(|conn| conn.upgrade = Some(str::from_utf8(s).expect("should be ascii").to_string()));
      st
    },
    HeaderValue::Cookie(cookies) => {
      let sticky_session_header = cookies.into_iter().find(|ref cookie| &cookie.name[..] == sticky_name.as_bytes());
      if let Some(sticky_session) = sticky_session_header {
        let mut st = state;
        st.get_mut_connection().map(|conn| conn.sticky_session = str::from_utf8(sticky_session.value).map(|s| s.to_string()).ok());

        return st;
      }

      state
    },
    HeaderValue::Error       => state.into_error()
  }
}

pub fn parse_header<'a>(buf: &'a mut Buffer, state: RequestState, sticky_name: &str) -> IResult<&'a [u8], RequestState> {
  match message_header(buf.data()) {
    Ok((i, header)) => Ok((i, validate_request_header(state, &header, sticky_name))),
    Err(e) => Err(e),
  }
}

pub fn parse_request(state: RequestState, buf: &[u8], sticky_name: &str) -> (BufferMove, RequestState) {
  match state {
    RequestState::Initial => {
      match request_line(buf) {
        Ok((i, r))    => {
          if let Some(rl) = RRequestLine::from_request_line(r) {

            let conn = Connection::new();
            //FIXME: what if it's not absolute path or complete URL, but an authority with CONNECT?
            if rl.uri.len() > 0 && rl.uri.as_bytes()[0] != b'/' {
              if let Some(host) = Url::parse(&rl.uri).ok().and_then(|u| u.host_str().map(|s| s.to_string())) {
                (BufferMove::Advance(buf.offset(i)), RequestState::HasHost(rl, conn, host))
              } else {
                (BufferMove::None, (RequestState::Initial).into_error())
              }
            } else {
              /*let conn = if rl.version == "11" {
                Connection::keep_alive()
              } else {
                Connection::close()
              };
              */
              (BufferMove::Advance(buf.offset(i)), RequestState::HasRequestLine(rl, conn))
            }
          } else {
            (BufferMove::None, (RequestState::Initial).into_error())
          }
        },
        res => default_request_result(state, res)
      }
    },
    RequestState::HasRequestLine(rl, conn) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasRequestLine(rl, conn), &header, sticky_name))
        },
        res => default_request_result(RequestState::HasRequestLine(rl, conn), res)
      }
    },
    RequestState::HasHost(rl, conn, h) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasHost(rl, conn, h), &header, sticky_name))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, RequestState::HasHost(rl, conn, h)),
        Err(_) => {
          match crlf(buf) {
            Ok((i, _)) => {
              (BufferMove::Advance(buf.offset(i)), RequestState::Request(rl, conn, h))
            },
            res => {
              //error!("PARSER\tHasHost could not parse header for input:\n{}\n", buf.to_hex(16));
              default_request_result(RequestState::HasHost(rl, conn, h), res)
            }
          }
        }
      }
    },
    RequestState::HasLength(rl, conn, l) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasLength(rl, conn, l), &header, sticky_name))
        },
        res => default_request_result(RequestState::HasLength(rl, conn, l), res)
      }
    },
    RequestState::HasHostAndLength(rl, conn, h, l) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasHostAndLength(rl, conn, h, l), &header, sticky_name))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, RequestState::HasHostAndLength(rl, conn, h, l)),
        Err(_) => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
                match l {
                  LengthInformation::Chunked    => (BufferMove::Advance(buf.offset(i)), RequestState::RequestWithBodyChunks(rl, conn, h, Chunk::Initial)),
                  LengthInformation::Length(sz) => (BufferMove::Advance(buf.offset(i)), RequestState::RequestWithBody(rl, conn, h, sz)),
                }
            },
            res => {
              error!("PARSER\tHasHostAndLength could not parse header for input:\n{}\n", buf.to_hex(16));
              default_request_result(RequestState::HasHostAndLength(rl, conn, h, l), res)
            }
          }
        }
      }
    },
    RequestState::RequestWithBodyChunks(rl, conn, h, ch) => {
      let (advance, chunk_state) = ch.parse(buf);
      //FIXME: should handle Chunk::Error here
      (advance, RequestState::RequestWithBodyChunks(rl, conn, h, chunk_state))
    },
    _ => {
      error!("PARSER\tunimplemented state: {:?}", state);
      (BufferMove::None, state.into_error())
    }
  }
}

pub fn default_response_result<O>(state: ResponseState, res: IResult<&[u8], O>) -> (BufferMove, ResponseState) {
  match res {
    Err(Err::Error(_)) | Err(Err::Failure(_)) => (BufferMove::None, state.into_error()),
    Err(Err::Incomplete(_)) => (BufferMove::None, state),
    _                      => unreachable!()
  }
}

pub fn validate_response_header(mut state: ResponseState, header: &Header, is_head: bool) -> ResponseState {
  match header.value() {
    HeaderValue::ContentLength(sz) => {
      match state {
        // if the request has a HEAD method, we don't count the content length
        // FIXME: what happens if multiple content lengths appear?
        ResponseState::HasStatusLine(sl, conn) => if is_head {
          ResponseState::HasStatusLine(sl, conn)
        } else {
          ResponseState::HasLength(sl, conn, LengthInformation::Length(sz))
        },
        s                                      => s.into_error(),
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        ResponseState::HasStatusLine(sl, conn) => if is_head {
          ResponseState::HasStatusLine(sl, conn)
        } else {
          ResponseState::HasLength(sl, conn, LengthInformation::Chunked)
        },
        s                                      => s.into_error(),
      }
    },
    // FIXME: for now, we don't remember if we cancel indications from a previous Connection Header
    HeaderValue::Connection(c) => {
      if state.get_mut_connection().map(|conn| {
        if c.has_close {
          conn.keep_alive = Some(false);
        }
        if c.has_keep_alive {
          conn.keep_alive = Some(true);
        }
        if c.has_upgrade {
          conn.has_upgrade = true;
        }
      }).is_some() {
        if let ResponseState::HasUpgrade(rl, conn, proto) = state {
          if conn.has_upgrade {
            ResponseState::HasUpgrade(rl, conn, proto)
          } else {
            ResponseState::Error(Some(rl), Some(conn), Some(proto), None, None)
          }
        } else {
          state
        }
      } else {
        state.into_error()
      }
    },
    HeaderValue::Upgrade(protocol) => {
      let proto = str::from_utf8(protocol).expect("the parsed protocol should be a valid utf8 string").to_string();
      trace!("parsed a protocol: {:?}", proto);
      trace!("state is {:?}", state);
      match state {
        ResponseState::HasStatusLine(sl, mut conn) => {
          conn.upgrade = Some(proto.clone());
          ResponseState::HasUpgrade(sl, conn, proto)
        },
        s                                       => s.into_error(),
      }
    }

    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => state.into_error(),
    HeaderValue::Host(_)     => state.into_error(),
    /*
    HeaderValue::Forwarded(_)  => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedFor(_) => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedProto(_) => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedPort(_) => ResponseState::Error(ErrorState::InvalidHttp),
    */
    HeaderValue::Forwarded   => state,
    HeaderValue::Other(_,_)  => state,
    HeaderValue::ExpectContinue => {
      // we should not get that one from the server
      state.into_error()
    },
    HeaderValue::Cookie(_)   => state,
    HeaderValue::Error       => state.into_error()
  }
}

pub fn parse_response(state: ResponseState, buf: &[u8], is_head: bool, sticky_name: &str) -> (BufferMove, ResponseState) {
  match state {
    ResponseState::Initial => {
      match status_line(buf) {
        Ok((i, r))    => {
          if let Some(rl) = RStatusLine::from_status_line(r) {
            let conn = Connection::new();
            /*let conn = if rl.version == "11" {
              Connection::keep_alive()
            } else {
              Connection::close()
            };
            */
            (BufferMove::Advance(buf.offset(i)), ResponseState::HasStatusLine(rl, conn))
          } else {
            (BufferMove::None, ResponseState::Error(None, None, None, None, None))
          }
        },
        res => default_response_result(state, res)
      }
    },
    ResponseState::HasStatusLine(sl, conn) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_response_header(ResponseState::HasStatusLine(sl, conn), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasStatusLine(sl, conn)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
              // no content
              if is_head ||
                // all 1xx responses
                sl.status / 100  == 1 || sl.status == 204 || sl.status == 304 {
                (BufferMove::Advance(buf.offset(i)), ResponseState::Response(sl, conn))
              } else {
                // no length information, so we'll assume that the response ends when the connection is closed
                (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBodyCloseDelimited(sl, conn, false))
              }
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasStatusLine(sl, conn), res)
            }
          }
        }
      }
    },
    ResponseState::HasLength(sl, conn, length) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv,  validate_response_header(ResponseState::HasLength(sl, conn, length), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasLength(sl, conn, length)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
                match length {
                  LengthInformation::Chunked    => (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBodyChunks(sl, conn, Chunk::Initial)),
                  LengthInformation::Length(sz) => (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBody(sl, conn, sz)),
                }
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasLength(sl, conn, length), res)
            }
          }
        }
      }
    },
    ResponseState::HasUpgrade(sl, conn, protocol) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_response_header(ResponseState::HasUpgrade(sl, conn, protocol), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasUpgrade(sl, conn, protocol)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
              (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseUpgrade(sl, conn, protocol))
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasUpgrade(sl, conn, protocol), res)
            }
          }
        }
      }
    },
    ResponseState::ResponseWithBodyChunks(rl, conn, ch) => {
      let (advance, chunk_state) = ch.parse(buf);
      (advance, ResponseState::ResponseWithBodyChunks(rl, conn, chunk_state))
    },
    ResponseState::ResponseWithBodyCloseDelimited(rl, conn, b) => {
      (BufferMove::Advance(buf.len()), ResponseState::ResponseWithBodyCloseDelimited(rl, conn, b))
    },
    _ => {
      error!("PARSER\tunimplemented state: {:?}", state);
      (BufferMove::None, state.into_error())
    }
  }
}

pub fn parse_request_until_stop(mut current_state: RequestState, mut header_end: Option<usize>,
  buf: &mut BufferQueue, added_req_header: &str, sticky_name: &str)
  -> (RequestState, Option<usize>) {
  loop {
    let (mv, new_state) = parse_request(current_state, buf.unparsed_data(), sticky_name);
    //println!("PARSER\t{}\tinput:\n{}\nmv: {:?}, new state: {:?}\n", request_id, &buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tinput:\n{}\nmv: {:?}, new state: {:?}\n", request_id, &buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tmv: {:?}, new state: {:?}\n", request_id, mv, new_state);
    current_state = new_state;

    match mv {
      BufferMove::Advance(sz) => {
        assert!(sz != 0, "buffer move should not be 0");
        //FIXME: what if we advance past the buffer's end? Splice?
        buf.consume_parsed_data(sz);
        if header_end.is_none() {
          match current_state {
            RequestState::Request(_,_,_) |
            RequestState::RequestWithBodyChunks(_,_,_,Chunk::Initial) => {
              //println!("FOUND HEADER END (advance):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.slice_output(sz);
            },
            RequestState::RequestWithBody(_,ref mut conn,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));

              // If we got "Expects: 100-continue", the body will be sent later
              if conn.continues == Continue::None {
                buf.slice_output(sz+content_length);
                buf.consume_parsed_data(content_length);
              } else {
                buf.slice_output(sz);
                conn.continues = Continue::Expects(content_length);
              }
            },
            _ => {
              buf.slice_output(sz);
            }
          }
        } else {
          buf.slice_output(sz);
        }
      },
      BufferMove::Delete(length) => {
        buf.consume_parsed_data(length);
        if header_end.is_none() {
          match current_state {
            RequestState::Request(_,_,_) |
            RequestState::RequestWithBodyChunks(_,_,_,_) => {
              //println!("FOUND HEADER END (delete):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.delete_output(length);
            },
            RequestState::RequestWithBody(_,_,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.delete_output(length);

              buf.slice_output(content_length);
              buf.consume_parsed_data(content_length);
            },
            _ => {
              buf.delete_output(length);
            }
          }
        } else {
          buf.delete_output(length);
        }
      },
      BufferMove::Multiple(buffer_moves) => {
        for buffer_move in buffer_moves {
          match buffer_move {
            BufferMove::Advance(length) => {
              buf.consume_parsed_data(length);
              buf.slice_output(length);
            },
            BufferMove::Delete(length) => {
              buf.consume_parsed_data(length);
              buf.delete_output(length);
            },
            e => {
              error!("BufferMove {:?} isn't implemented", e);
              unimplemented!();
            }
          }
        }
      }
      _ => break
    }

    match current_state {
      RequestState::Error(_,_,_,_,_) => {
        incr!("http1.parser.request.error");
        break;
      },
      RequestState::Request(_,_,_) | RequestState::RequestWithBody(_,_,_,_) |
        RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended) => break,
      _ => ()
    }
  }

  (current_state, header_end)
}

pub fn parse_response_until_stop(mut current_state: ResponseState, mut header_end: Option<usize>,
    buf: &mut BufferQueue, is_head: bool, added_res_header: &str,
    sticky_name: &str, sticky_session: Option<&StickySession>)
  -> (ResponseState, Option<usize>) {
  loop {
    //trace!("PARSER\t{}\tpos[{}]: {:?}", request_id, position, current_state);
    let (mv, new_state) = parse_response(current_state, buf.unparsed_data(), is_head, sticky_name);
    //trace!("PARSER\tinput:\n{}\nmv: {:?}, new state: {:?}\n", buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tmv: {:?}, new state: {:?}\n", request_id, mv, new_state);
    current_state = new_state;

    match mv {
      BufferMove::Advance(sz) => {
        assert!(sz != 0, "buffer move should not be 0");

        // header_end is some if we already parsed the headers
        if header_end.is_none() {
          match current_state {
            ResponseState::Response(_,_) |
            ResponseState::ResponseUpgrade(_,_,_) |
            ResponseState::ResponseWithBodyChunks(_,_,_) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz);
            },
            ResponseState::ResponseWithBody(_,_,content_length) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz+content_length);
              buf.consume_parsed_data(content_length);
            },
            ResponseState::ResponseWithBodyCloseDelimited(_,ref conn, _) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              // special case: some servers send responses with no body,
              // no content length, and Connection: close
              // since we deleted the Connection header, we'll add a new one
              if conn.keep_alive == Some(false) {
                buf.insert_output(Vec::from(&b"Connection: close\r\n"[..]));
              }

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz);

              let len = buf.available_input_data();
              buf.consume_parsed_data(len);
              buf.slice_output(len);
            },
            _ => {
              buf.consume_parsed_data(sz);
              buf.slice_output(sz);
            }
          }
        } else {
          buf.consume_parsed_data(sz);
          buf.slice_output(sz);
        }
        //FIXME: if we add a slice here, we will get a first large slice, then a long list of buffer size slices added by the slice_input function
      },
      BufferMove::Delete(length) => {
        buf.consume_parsed_data(length);
        if header_end.is_none() {
          match current_state {
            ResponseState::Response(_,_) |
            ResponseState::ResponseUpgrade(_,_,_) |
            ResponseState::ResponseWithBodyChunks(_,_,_) => {
              //println!("FOUND HEADER END (delete):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.delete_output(length);
            },
            ResponseState::ResponseWithBody(_,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              buf.delete_output(length);

              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.slice_output(content_length);
              buf.consume_parsed_data(content_length);
            },
            _ => {
              buf.delete_output(length);
            }
          }
        } else {
          buf.delete_output(length);
        }
      },
      _ => break
    }

    match current_state {
      ResponseState::Error(_,_,_,_,_) => {
        incr!("http1.parser.response.error");
        break;
      }
      ResponseState::Response(_,_) | ResponseState::ResponseWithBody(_,_,_) |
        ResponseState::ResponseUpgrade(_,_,_) |
        ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended) |
        ResponseState::ResponseWithBodyCloseDelimited(_,_,_) => break,
      _ => ()
    }
    //println!("move: {:?}, new state: {:?}, input_queue {:?}, output_queue: {:?}", mv, current_state, buf.input_queue, buf.output_queue);
  }

  //println!("end state: {:?}, input_queue {:?}, output_queue: {:?}", current_state, buf.input_queue, buf.output_queue);
  (current_state, header_end)
}

fn add_sticky_session_to_response(buf: &mut BufferQueue,
  sticky_name: &str, sticky_session: Option<&StickySession>) {
  if let Some(ref sticky_backend) = sticky_session {
    let sticky_cookie = format!("Set-Cookie: {}={}; Path=/\r\n", sticky_name, sticky_backend.sticky_id);
    buf.insert_output(Vec::from(sticky_cookie.as_bytes()));
  }
}
