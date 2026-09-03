//! Client for the **uictl** input broker.
//!
//! muvor never touches `/dev/uinput` and never reads `/dev/input/event*`;
//! every pointer movement and click in the product goes through this crate,
//! over one unix socket, to a broker that holds those privileges alone.
//! That is uictl's invariant 7 applied to the client, and it is what stops
//! muvor being a keylogger by accident.
//!
//! `WIRE.md` in the uictl checkout is normative. Section numbers in comments
//! here refer to it.
//!
//! This crate has no dependencies and needs no display, session bus or
//! daemon to test — see `tests/vectors.rs`, which runs against uictl's own
//! conformance vectors.

pub mod validate;
pub mod wire;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use wire::{
    BatchItem, Button, EncodeError, HelloResponse, Op, Request, ResultCode, SourceTag,
};

/// The name muvor claims at the handshake, and the name that must appear in
/// `~/.config/uictl/clients` for it to be granted the `interactive` class.
/// `WIRE.md` §3.5: a label, not a credential — the daemon binds it to a
/// binary path with `exe=`.
pub const CLIENT_NAME: &str = "muvor";

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Encode(EncodeError),
    /// The daemon answered, and refused.
    Refused { op: Op, result: ResultCode },
    /// A reply whose header did not match the request it claims to answer.
    /// `WIRE.md` §2.4: a response is the request's header echoed.
    Mismatched,
    /// A HELLO response shorter than the 24-byte floor of §3.4.
    ShortHello,
    /// `reserved` was non-zero. §3.4 keeps it as an explicit canary against
    /// uninitialised tail padding reaching the wire; if it ever trips, the
    /// daemon is leaking process memory and this is not a warning.
    ReservedNonZero,
    /// The daemon does not implement an opcode muvor requires.
    Unsupported(Op),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self { Self::Io(e) }
}
impl From<EncodeError> for Error {
    fn from(e: EncodeError) -> Self { Self::Encode(e) }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "uictl socket: {e}"),
            Self::Encode(e) => write!(f, "cannot encode request: {e:?}"),
            Self::Refused { op, result } => {
                write!(f, "uictl refused {op:?}: result {}", result.0)?;
                // The two refusals that mean "your setup is wrong", not "your
                // request was wrong". Naming the fix here is the difference
                // between a bug report and a five-second correction.
                if *result == ResultCode::ERR_DENIED_BY_POLICY {
                    write!(
                        f,
                        "\n  hint: add '{CLIENT_NAME} interactive exe=<abs path>' \
                         to ~/.config/uictl/clients"
                    )?;
                }
                Ok(())
            }
            Self::Mismatched => write!(f, "uictl reply did not echo the request header"),
            Self::ShortHello => write!(f, "HELLO response shorter than 24 bytes"),
            Self::ReservedNonZero => {
                write!(f, "HELLO reserved field non-zero — daemon may be leaking memory")
            }
            Self::Unsupported(op) => write!(f, "daemon does not implement {op:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// `WIRE.md` §1.1.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe_uid())));
    dir.join("uictld.sock")
}

fn unsafe_uid() -> u32 {
    // No libc dependency for one number: /proc/self/loginuid is unreliable,
    // but the runtime dir is nearly always set under a session. This is the
    // fallback for the case where it is not.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|l| l.split_whitespace().next().map(str::to_owned))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// A connected, handshaken client.
pub struct Client {
    sock: UnixStream,
    seq: u32,
    source_tag: SourceTag,
    hello: HelloResponse,
    /// One frame buffer for the process lifetime. Doctrine: nothing is
    /// allocated on the hot path.
    buf: Vec<u8>,
}

impl Client {
    /// Connect and complete the handshake. `WIRE.md` §5A.6 order of
    /// operations, with nothing omitted.
    pub fn connect() -> Result<Self, Error> {
        Self::connect_at(&default_socket_path())
    }

    pub fn connect_at(path: &Path) -> Result<Self, Error> {
        let sock = UnixStream::connect(path)?;
        // A hung daemon must not become a hung hotkey. The read timeout is
        // the only thing standing between a stuck broker and a frozen
        // keypress path.
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        sock.set_write_timeout(Some(Duration::from_millis(500)))?;

        let mut c = Self {
            sock,
            seq: 1,
            source_tag: SourceTag::HOTKEY,
            hello: HelloResponse {
                proto_selected: 0,
                device_caps: 0,
                abs_range_max: 0,
                opcode_bitmap: 0,
                daemon_version: 0,
                reconnect: None,
            },
            buf: vec![0u8; wire::HEADER_LEN + wire::MAX_PAYLOAD],
        };
        c.handshake()?;
        Ok(c)
    }

    fn handshake(&mut self) -> Result<(), Error> {
        let body = self.roundtrip(Request::Hello {
            proto_min: wire::PROTO_VERSION,
            proto_max: wire::PROTO_VERSION,
            client_name: CLIENT_NAME,
        })?;
        if body.len() < HelloResponse::MIN_LEN {
            return Err(Error::ShortHello);
        }
        if body[20..24] != [0, 0, 0, 0] {
            return Err(Error::ReservedNonZero);
        }
        self.hello = HelloResponse::decode(&body).ok_or(Error::ShortHello)?;

        // §3.4: gate every feature on opcode_bitmap, NEVER on daemon_version.
        for op in [Op::MoveAbs, Op::Button, Op::Batch] {
            if !self.hello.supports(op) {
                return Err(Error::Unsupported(op));
            }
        }
        Ok(())
    }

    pub fn hello(&self) -> &HelloResponse { &self.hello }

    /// `WIRE.md` §3.4 / §5A.1 — the coordinate contract. muvor converts.
    pub fn abs_range_max(&self) -> u32 { self.hello.abs_range_max }

    pub fn set_source_tag(&mut self, tag: SourceTag) { self.source_tag = tag; }

    pub fn ping(&mut self) -> Result<(), Error> {
        self.roundtrip(Request::Ping).map(drop)
    }

    /// Device units, `0..abs_range_max`. Out of range is clamped by the
    /// daemon, not refused (§5A.1) — the same thing a real pointer does at
    /// the screen edge.
    pub fn move_abs(&mut self, x: i32, y: i32) -> Result<(), Error> {
        self.roundtrip(Request::MoveAbs { x, y }).map(drop)
    }

    pub fn button(&mut self, code: Button, down: bool) -> Result<(), Error> {
        self.roundtrip(Request::Button { code, down }).map(drop)
    }

    /// A release that treats "it was not held" as the outcome it wanted.
    ///
    /// `ERR_KEY_NOT_HELD` says the button is up, which is the entire purpose
    /// of the call — and letting go is the one operation that must never fail
    /// on its way out of a mode (§12.9). It arrives whenever muvor's model
    /// and uictl's have already been reconciled by something else: a
    /// [`Self::unstick`] repair, or the release burst uictl synthesizes when
    /// a client disconnects.
    pub fn release(&mut self, code: Button) -> Result<(), Error> {
        match self.button(code, false) {
            Err(Error::Refused { result, .. }) if result == ResultCode::ERR_KEY_NOT_HELD => Ok(()),
            other => other,
        }
    }

    /// A press that repairs a stale hold rather than failing on it — see
    /// [`Self::unstick`]. Only a *press* can be stale in this way: a release
    /// that uictl refuses has already achieved what it was for.
    pub fn press(&mut self, code: Button) -> Result<(), Error> {
        match self.button(code, true) {
            Err(Error::Refused { result, .. }) if result == ResultCode::ERR_KEY_ALREADY_HELD => {
                self.unstick(code)?;
                self.button(code, true)
            }
            other => other,
        }
    }

    /// `WIRE.md` §5A.2 — out of range is an **error** here, not a clamp, and
    /// the difference from `MOVE_ABS` is the point: a relative delta has no
    /// natural ceiling to clamp to.
    pub fn move_rel(&mut self, dx: i32, dy: i32) -> Result<(), Error> {
        self.roundtrip(Request::MoveRel { dx, dy }).map(drop)
    }

    pub fn scroll(&mut self, notches_v: i32, notches_h: i32) -> Result<(), Error> {
        self.roundtrip(Request::Scroll { notches_v, notches_h }).map(drop)
    }

    pub fn batch(&mut self, items: &[BatchItem]) -> Result<(), Error> {
        self.roundtrip(Request::Batch(items)).map(drop)
    }

    /// A hint jump is one batch and therefore **one rate-limit token**, not
    /// three (`WIRE.md` §5B.4, and uictl's budget in plan.md §3.4). Sending
    /// these as three frames is how a 50/s budget becomes a 16/s one.
    pub fn click_at(&mut self, x: i32, y: i32, code: Button) -> Result<(), Error> {
        let click = [
            BatchItem::MoveAbs { x, y },
            BatchItem::Button { code, down: true },
            BatchItem::Button { code, down: false },
        ];
        match self.batch(&click) {
            Err(Error::Refused { result, .. }) if result == ResultCode::ERR_KEY_ALREADY_HELD => {
                self.unstick(code)?;
                self.batch(&click)
            }
            other => other,
        }
    }

    /// Let go of a button uictl says this connection is holding, and say so.
    ///
    /// **Measured on the real desktop 2026-08-26**: movement mode pressed the
    /// left button for a drag and returned on an error before releasing it,
    /// and because the daemon keeps one uictl connection for its whole life,
    /// *every click after that was refused* — `ERR_KEY_ALREADY_HELD`, in
    /// every window, until the daemon was restarted. From the desk it looked
    /// like `f`, `d`, `g`, `a` and `r` had simply stopped existing, and the
    /// desktop was quietly in a drag the whole time.
    ///
    /// Repairing it here rather than at the call site is deliberate: the
    /// disagreement is between muvor's model and uictl's, and every path that
    /// clicks — `hint` included — is equally exposed to it.
    fn unstick(&mut self, code: Button) -> Result<(), Error> {
        eprintln!(
            "muvor: uictl says the {code:?} button is already held — releasing it and retrying"
        );
        self.release(code)
    }

    fn roundtrip(&mut self, req: Request<'_>) -> Result<Vec<u8>, Error> {
        let op = req.opcode();
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);

        let n = req.encode_into(&mut self.buf, self.source_tag, seq)?;
        self.sock.write_all(&self.buf[..n])?;

        let mut head = [0u8; wire::HEADER_LEN];
        self.sock.read_exact(&mut head)?;
        let h = wire::Header::decode(&head).ok_or(Error::Mismatched)?;

        // §2.4: the response echoes version, opcode, source_tag and seq.
        if h.opcode != op as u16 || h.seq != seq || h.source_tag != self.source_tag.0 {
            return Err(Error::Mismatched);
        }
        if h.payload_len < 2 || h.payload_len as usize > wire::MAX_PAYLOAD {
            return Err(Error::Mismatched);
        }

        let mut payload = vec![0u8; h.payload_len as usize];
        self.sock.read_exact(&mut payload)?;
        let result = ResultCode(u16::from_le_bytes([payload[0], payload[1]]));
        if !result.is_ok() {
            return Err(Error::Refused { op, result });
        }
        // §2.4: response data grows append-only; accept a longer tail.
        Ok(payload[2..].to_vec())
    }
}
