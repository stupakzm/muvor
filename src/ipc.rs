//! The muvor socket wire (§2.5, M5s-a).
//!
//! Hand-rolled and length-prefixed, for the same reason uictl's wire is
//! (§3.1): this protocol has four messages and one client at a time, and a
//! serialisation dependency would be more code than the thing it serialises.
//!
//! The shape is deliberately dull. A frame is a `u32` length and that many
//! bytes; the first byte of the payload is a tag; a string is a `u32` length
//! and that many UTF-8 bytes. Everything is little-endian because both ends
//! are the same machine — this socket is never a network.
//!
//! **What goes over it is a whole command, not a query.** The client sends
//! "run a hint with these flags" and receives the lines the daemon printed
//! while running it. That is what makes the client thin: every connection,
//! every subscription and every measurement stays on the daemon's side, and
//! M5s-b and -c can move state into it without the protocol noticing.

use std::io::{Read, Write};

/// Refuse a frame larger than this rather than allocate what it asks for.
/// A hint's whole output is a few kilobytes; anything near this is a bug or
/// a stranger.
const MAX_FRAME: u32 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Is anyone there, and running which build?
    Ping,
    /// Run the hint loop and stream what it prints.
    ///
    /// `measure` stops after the timing block: the overlay is taken down
    /// immediately instead of waiting for a label. It exists because the
    /// numbers §5.2 budgets are all printed *before* a human is involved, so
    /// sampling them should not cost a keystroke or ten seconds of somebody's
    /// keyboard (§2.5, M5s-d).
    Hint { after: u64, dry_run: bool, explain: bool, measure: bool, deep: bool },
    /// What the daemon is holding: the mirror, per application (§2.5, M5s-c).
    ///
    /// Without this the mirror is unobservable from outside the daemon —
    /// `muvor dump` runs in its own process and has its own empty one — so
    /// "the mirror did not answer" and "the mirror was never warmed" look
    /// identical, and this project has already paid for that confusion twice.
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Pong { version: String },
    /// One line of output, without its newline.
    Line(String),
    /// The command finished. `None` is success; `Some` carries the error the
    /// one-shot would have printed to stderr.
    Done(Option<String>),
}

const T_PING: u8 = 1;
const T_HINT: u8 = 2;
const T_STATUS: u8 = 6;
const T_PONG: u8 = 3;
const T_LINE: u8 = 4;
const T_DONE: u8 = 5;

impl Request {
    fn payload(&self) -> Vec<u8> {
        let mut v = Vec::new();
        match self {
            Self::Ping => v.push(T_PING),
            Self::Status => v.push(T_STATUS),
            Self::Hint { after, dry_run, explain, measure, deep } => {
                v.push(T_HINT);
                v.extend_from_slice(&after.to_le_bytes());
                v.push(u8::from(*dry_run));
                v.push(u8::from(*explain));
                v.push(u8::from(*measure));
                v.push(u8::from(*deep));
            }
        }
        v
    }

    fn parse(b: &[u8]) -> Result<Self, Error> {
        match b.first() {
            Some(&T_PING) => Ok(Self::Ping),
            Some(&T_STATUS) => Ok(Self::Status),
            Some(&T_HINT) if b.len() >= 11 => {
                let after = u64::from_le_bytes(b[1..9].try_into().expect("8 bytes"));
                // Trailing bytes are read if they are there and defaulted
                // if they are not: a daemon left running across an upgrade
                // still understands the client that gained `--measure`, and
                // then `--deep`, and says no to each. The alternative is a
                // version handshake for one bool, twice.
                Ok(Self::Hint {
                    after,
                    dry_run: b[9] != 0,
                    explain: b[10] != 0,
                    measure: b.get(11).is_some_and(|&m| m != 0),
                    deep: b.get(12).is_some_and(|&d| d != 0),
                })
            }
            _ => Err(Error::Malformed),
        }
    }
}

impl Response {
    fn payload(&self) -> Vec<u8> {
        let mut v = Vec::new();
        match self {
            Self::Pong { version } => {
                v.push(T_PONG);
                put_str(&mut v, version);
            }
            Self::Line(s) => {
                v.push(T_LINE);
                put_str(&mut v, s);
            }
            Self::Done(err) => {
                v.push(T_DONE);
                put_str(&mut v, err.as_deref().unwrap_or(""));
                // An empty error and no error are different things, and the
                // difference decides an exit code.
                v.push(u8::from(err.is_some()));
            }
        }
        v
    }

    fn parse(b: &[u8]) -> Result<Self, Error> {
        match b.first() {
            Some(&T_PONG) => Ok(Self::Pong { version: take_str(&b[1..])?.0 }),
            Some(&T_LINE) => Ok(Self::Line(take_str(&b[1..])?.0)),
            Some(&T_DONE) => {
                let (s, used) = take_str(&b[1..])?;
                let failed = b.get(1 + used).copied().unwrap_or(0) != 0;
                Ok(Self::Done(failed.then_some(s)))
            }
            _ => Err(Error::Malformed),
        }
    }
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    let n = u32::try_from(s.len()).unwrap_or(u32::MAX);
    v.extend_from_slice(&n.to_le_bytes());
    v.extend_from_slice(&s.as_bytes()[..n as usize]);
}

fn take_str(b: &[u8]) -> Result<(String, usize), Error> {
    if b.len() < 4 {
        return Err(Error::Malformed);
    }
    let n = u32::from_le_bytes(b[0..4].try_into().expect("4 bytes")) as usize;
    let end = 4usize.checked_add(n).ok_or(Error::Malformed)?;
    if b.len() < end {
        return Err(Error::Malformed);
    }
    Ok((String::from_utf8_lossy(&b[4..end]).into_owned(), end))
}

#[derive(Debug)]
pub enum Error {
    /// The peer closed cleanly between frames — normal, not a failure.
    Closed,
    Malformed,
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the other end closed the socket"),
            Self::Malformed => write!(f, "malformed frame"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn write_frame(w: &mut impl Write, payload: &[u8]) -> Result<(), Error> {
    let n = u32::try_from(payload.len()).map_err(|_| Error::Malformed)?;
    w.write_all(&n.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, Error> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
        Err(e) => return Err(Error::Io(e)),
    }
    let n = u32::from_le_bytes(len);
    if n > MAX_FRAME {
        return Err(Error::Malformed);
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn send_request(w: &mut impl Write, r: &Request) -> Result<(), Error> {
    write_frame(w, &r.payload())
}

pub fn read_request(r: &mut impl Read) -> Result<Request, Error> {
    Request::parse(&read_frame(r)?)
}

pub fn send_response(w: &mut impl Write, r: &Response) -> Result<(), Error> {
    write_frame(w, &r.payload())
}

pub fn read_response(r: &mut impl Read) -> Result<Response, Error> {
    Response::parse(&read_frame(r)?)
}

/// Where the socket lives.
///
/// `$XDG_RUNTIME_DIR` because it is per-user, `tmpfs`, and **cleared when the
/// session ends** — which is the right lifetime for a thing that talks to a
/// compositor and a uinput daemon, neither of which survives a logout either.
pub fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    std::path::Path::new(&dir).join("muvor.sock")
}

/// An [`std::io::Write`] that turns a command's printed output into one
/// [`Response::Line`] per line.
///
/// The daemon runs the same code the one-shot runs, writing to this instead
/// of stdout — which is what makes "the daemon prints what the one-shot
/// prints" a property of the transport rather than a thing to keep in step
/// by hand.
pub struct Lines<W: Write> {
    out: W,
    buf: Vec<u8>,
}

impl<W: Write> Lines<W> {
    pub fn new(out: W) -> Self {
        Self { out, buf: Vec::with_capacity(256) }
    }

    /// Emit whatever is left without a trailing newline.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            send_response(&mut self.out, &Response::Line(line))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

impl<W: Write> Write for Lines<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        for &byte in data {
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                send_response(&mut self.out, &Response::Line(line))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            } else {
                self.buf.push(byte);
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_request(r: Request) {
        let mut wire = Vec::new();
        send_request(&mut wire, &r).expect("send");
        assert_eq!(read_request(&mut wire.as_slice()).expect("read"), r);
    }

    #[test]
    fn requests_survive_the_wire() {
        round_trip_request(Request::Ping);
        round_trip_request(Request::Status);
        round_trip_request(Request::Hint {
            after: 5, dry_run: true, explain: false, measure: false, deep: false,
        });
        round_trip_request(Request::Hint {
            after: 0, dry_run: false, explain: true, measure: false, deep: true,
        });
        round_trip_request(Request::Hint {
            after: 0, dry_run: true, explain: true, measure: true, deep: true,
        });
    }

    /// A client that predates `--measure` sends an 11-byte Hint payload,
    /// and one that predates `--deep` sends 12. Both must still parse, each
    /// as a hint that does not ask for what it has never heard of. This is
    /// the whole of the compatibility story and it is two `get`s.
    #[test]
    fn a_hint_frame_missing_its_trailing_bytes_still_parses() {
        let mut old = vec![T_HINT];
        old.extend_from_slice(&7u64.to_le_bytes());
        old.push(1);
        old.push(0);
        assert_eq!(old.len(), 11);
        assert_eq!(
            Request::parse(&old).expect("11-byte hint"),
            Request::Hint { after: 7, dry_run: true, explain: false, measure: false, deep: false }
        );

        let mut pre_deep = old.clone();
        pre_deep.push(1);
        assert_eq!(pre_deep.len(), 12);
        assert_eq!(
            Request::parse(&pre_deep).expect("12-byte hint"),
            Request::Hint { after: 7, dry_run: true, explain: false, measure: true, deep: false }
        );
    }

    #[test]
    fn responses_survive_the_wire() {
        for r in [
            Response::Pong { version: "0.1.0".into() },
            Response::Line(String::new()),
            Response::Line("typed            aa  after 9373 ms".into()),
            Response::Done(None),
            Response::Done(Some("uictl is not answering".into())),
        ] {
            let mut wire = Vec::new();
            send_response(&mut wire, &r).expect("send");
            assert_eq!(read_response(&mut wire.as_slice()).expect("read"), r);
        }
    }

    /// The distinction an exit code is made of: a command that failed with an
    /// empty message is not a command that succeeded.
    #[test]
    fn an_empty_error_is_still_an_error() {
        let mut wire = Vec::new();
        send_response(&mut wire, &Response::Done(Some(String::new()))).expect("send");
        assert_eq!(read_response(&mut wire.as_slice()).expect("read"), Response::Done(Some(String::new())));
    }

    #[test]
    fn a_clean_close_between_frames_is_not_an_error() {
        let empty: Vec<u8> = Vec::new();
        assert!(matches!(read_request(&mut empty.as_slice()), Err(Error::Closed)));
    }

    #[test]
    fn a_truncated_frame_is_refused_rather_than_waited_on() {
        let mut wire = Vec::new();
        send_response(&mut wire, &Response::Line("hello".into())).expect("send");
        wire.truncate(wire.len() - 2);
        assert!(read_response(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn an_absurd_length_is_refused_before_it_is_allocated() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(read_response(&mut wire.as_slice()), Err(Error::Malformed)));
    }

    #[test]
    fn printed_output_becomes_one_frame_per_line() {
        let mut lines = Lines::new(Vec::new());
        write!(lines, "first\nsecond\n").expect("write");
        write!(lines, "partial").expect("write");
        lines.finish().expect("finish");

        let wire = lines.into_inner();
        let mut r = wire.as_slice();
        assert_eq!(read_response(&mut r).unwrap(), Response::Line("first".into()));
        assert_eq!(read_response(&mut r).unwrap(), Response::Line("second".into()));
        assert_eq!(read_response(&mut r).unwrap(), Response::Line("partial".into()));
    }

    /// A blank line is output too — the hint's layout depends on them.
    #[test]
    fn blank_lines_are_kept() {
        let mut lines = Lines::new(Vec::new());
        write!(lines, "a\n\nb\n").expect("write");
        let wire = lines.into_inner();
        let mut r = wire.as_slice();
        assert_eq!(read_response(&mut r).unwrap(), Response::Line("a".into()));
        assert_eq!(read_response(&mut r).unwrap(), Response::Line(String::new()));
        assert_eq!(read_response(&mut r).unwrap(), Response::Line("b".into()));
    }
}
