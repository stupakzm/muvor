//! Frame encoding and decoding. `WIRE.md` §2 is normative; every constant
//! here cites the section it comes from.

/// `WIRE.md` §2.1 — exactly 16 bytes, `u16 u16 u32 u32 u32`, no padding.
pub const HEADER_LEN: usize = 16;

/// `WIRE.md` §2.1 — `UICTL_MAX_PAYLOAD`. The most attacker-controlled field
/// in the protocol; bounded before it is used as a length, on both sides.
pub const MAX_PAYLOAD: usize = 4096;

/// `WIRE.md` §3.1 — `UICTL_CLIENT_NAME_MAX`. Fixed width, NUL-padded.
pub const CLIENT_NAME_MAX: usize = 32;

/// The only version muvor speaks. `WIRE.md` §3.3.
pub const PROTO_VERSION: u16 = 1;

/// `WIRE.md` §2.2. Values are on the wire: append only, never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Op {
    Invalid = 0,
    Ping = 1,
    MoveAbs = 2,
    Hello = 3,
    KeyTap = 4,
    KeySequence = 5,
    KeyDown = 6,
    KeyUp = 7,
    ConfirmSubscribe = 8,
    ConfirmRequest = 9,
    ConfirmDecide = 10,
    Button = 11,
    MoveRel = 12,
    Scroll = 13,
    Batch = 14,
}

impl Op {
    pub fn from_u16(v: u16) -> Option<Self> {
        use Op::*;
        Some(match v {
            0 => Invalid, 1 => Ping, 2 => MoveAbs, 3 => Hello, 4 => KeyTap,
            5 => KeySequence, 6 => KeyDown, 7 => KeyUp, 8 => ConfirmSubscribe,
            9 => ConfirmRequest, 10 => ConfirmDecide, 11 => Button,
            12 => MoveRel, 13 => Scroll, 14 => Batch,
            _ => return None,
        })
    }
}

/// `WIRE.md` §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultCode(pub u16);

impl ResultCode {
    pub const OK: Self = Self(0);
    pub const ERR_VERSION_UNSUPPORTED: Self = Self(1);
    pub const ERR_OPCODE_UNKNOWN: Self = Self(2);
    pub const ERR_PAYLOAD_INVALID: Self = Self(3);
    pub const ERR_DENIED_BY_POLICY: Self = Self(4);
    pub const ERR_TOO_LARGE: Self = Self(5);
    pub const ERR_HANDSHAKE_REQUIRED: Self = Self(8);
    pub const ERR_RATE_LIMITED: Self = Self(11);
    /// **This connection already holds that button.** uictl seeds a batch's
    /// validation from the connection's held set, and muvor's connection
    /// lives as long as the daemon — so one press whose release was lost
    /// refuses every click that follows, for hours, in every window.
    pub const ERR_KEY_ALREADY_HELD: Self = Self(12);
    pub const ERR_KEY_NOT_HELD: Self = Self(14);

    pub fn is_ok(self) -> bool { self == Self::OK }
}

/// `WIRE.md` §2.5 — **advisory only**. The daemon must never key a decision
/// on it, and muvor must never expect it to. It is a hint for the audit log.
#[derive(Debug, Clone, Copy)]
pub struct SourceTag(pub u32);

impl SourceTag {
    pub const CLI: Self = Self(1);
    pub const HOTKEY: Self = Self(2);
    pub const LLM: Self = Self(4);
}

/// `WIRE.md` §2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub opcode: u16,
    pub source_tag: u32,
    pub seq: u32,
    pub payload_len: u32,
}

impl Header {
    pub fn encode_into(&self, buf: &mut [u8; HEADER_LEN]) {
        buf[0..2].copy_from_slice(&self.version.to_le_bytes());
        buf[2..4].copy_from_slice(&self.opcode.to_le_bytes());
        buf[4..8].copy_from_slice(&self.source_tag.to_le_bytes());
        buf[8..12].copy_from_slice(&self.seq.to_le_bytes());
        buf[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Self {
            version: u16::from_le_bytes([buf[0], buf[1]]),
            opcode: u16::from_le_bytes([buf[2], buf[3]]),
            source_tag: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            seq: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            payload_len: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }
}

/// `WIRE.md` §5A.4 — the five codes the pointer device registers, and no
/// others. Keycodes valid for §5B are refused here by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Button {
    Left = 272,
    Right = 273,
    Middle = 274,
    Side = 275,
    Extra = 276,
}

/// `WIRE.md` §5B.4 — a batch item is a fixed-size tagged union, 12 bytes.
/// Exactly six sub-opcodes are batchable.
#[derive(Debug, Clone, Copy)]
pub enum BatchItem {
    MoveAbs { x: i32, y: i32 },
    MoveRel { dx: i32, dy: i32 },
    Scroll { notches_v: i32, notches_h: i32 },
    Button { code: Button, down: bool },
    KeyDown { keycode: u16 },
    KeyUp { keycode: u16 },
}

impl BatchItem {
    fn parts(self) -> (Op, i32, i32) {
        match self {
            Self::MoveAbs { x, y } => (Op::MoveAbs, x, y),
            Self::MoveRel { dx, dy } => (Op::MoveRel, dx, dy),
            Self::Scroll { notches_v, notches_h } => (Op::Scroll, notches_v, notches_h),
            Self::Button { code, down } => (Op::Button, code as i32, i32::from(down)),
            Self::KeyDown { keycode } => (Op::KeyDown, i32::from(keycode), 0),
            Self::KeyUp { keycode } => (Op::KeyUp, i32::from(keycode), 0),
        }
    }

    fn encode_into(self, out: &mut [u8]) {
        let (op, a, b) = self.parts();
        out[0..2].copy_from_slice(&(op as u16).to_le_bytes());
        out[2..4].copy_from_slice(&0u16.to_le_bytes()); // reserved, MUST be zero
        out[4..8].copy_from_slice(&a.to_le_bytes());
        out[8..12].copy_from_slice(&b.to_le_bytes());
    }
}

/// `WIRE.md` §5B.4 — `count` is 1..=16.
pub const BATCH_MAX_ITEMS: usize = 16;

/// The body of a request, before it is wrapped in a header.
#[derive(Debug, Clone)]
pub enum Request<'a> {
    Ping,
    Hello { proto_min: u16, proto_max: u16, client_name: &'a str },
    MoveAbs { x: i32, y: i32 },
    MoveRel { dx: i32, dy: i32 },
    Scroll { notches_v: i32, notches_h: i32 },
    Button { code: Button, down: bool },
    Batch(&'a [BatchItem]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// A client name that is empty, over 31 bytes, or not printable ASCII.
    /// `WIRE.md` §3.5 — the daemon rejects these, so we never emit one.
    BadClientName,
    /// `WIRE.md` §5B.4 — count must be 1..=16.
    BatchSize,
    /// `WIRE.md` §5A.2 / §5A.3 — a zero nudge or zero scroll is a client bug
    /// and the daemon refuses it. Refusing locally turns a round trip into a
    /// compile-adjacent error.
    ZeroDelta,
    /// `WIRE.md` §5A.3 — bounds are +/-1000 per axis.
    ScrollRange,
    /// Buffer too small for the frame.
    BufferTooSmall,
}

impl Request<'_> {
    pub fn opcode(&self) -> Op {
        match self {
            Self::Ping => Op::Ping,
            Self::Hello { .. } => Op::Hello,
            Self::MoveAbs { .. } => Op::MoveAbs,
            Self::MoveRel { .. } => Op::MoveRel,
            Self::Scroll { .. } => Op::Scroll,
            Self::Button { .. } => Op::Button,
            Self::Batch(_) => Op::Batch,
        }
    }

    /// Write the whole frame — header then payload — into `buf`, returning
    /// its length. Takes a caller-owned buffer so the hot path allocates
    /// nothing (doctrine: Efficient).
    pub fn encode_into(
        &self,
        buf: &mut [u8],
        source_tag: SourceTag,
        seq: u32,
    ) -> Result<usize, EncodeError> {
        let payload_len = self.write_payload(buf.get_mut(HEADER_LEN..).unwrap_or(&mut []))?;
        let mut head = [0u8; HEADER_LEN];
        Header {
            // WIRE.md §3.1: HELLO is the version-invariant bootstrap frame,
            // but muvor only ever speaks v1, so it is stamped like the rest.
            version: PROTO_VERSION,
            opcode: self.opcode() as u16,
            source_tag: source_tag.0,
            seq,
            payload_len: payload_len as u32,
        }
        .encode_into(&mut head);
        buf[..HEADER_LEN].copy_from_slice(&head);
        Ok(HEADER_LEN + payload_len)
    }

    fn write_payload(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let need = match self {
            Self::Ping => 0,
            Self::Hello { .. } => 4 + CLIENT_NAME_MAX,
            Self::MoveAbs { .. } | Self::MoveRel { .. } | Self::Scroll { .. } => 8,
            Self::Button { .. } => 4,
            Self::Batch(items) => {
                if items.is_empty() || items.len() > BATCH_MAX_ITEMS {
                    return Err(EncodeError::BatchSize);
                }
                4 + 12 * items.len()
            }
        };
        if out.len() < need {
            return Err(EncodeError::BufferTooSmall);
        }

        match *self {
            Self::Ping => {}
            Self::Hello { proto_min, proto_max, client_name } => {
                if !valid_client_name(client_name) {
                    return Err(EncodeError::BadClientName);
                }
                out[0..2].copy_from_slice(&proto_min.to_le_bytes());
                out[2..4].copy_from_slice(&proto_max.to_le_bytes());
                // WIRE.md §3.1: fixed width, NUL-terminated, and every byte
                // after the NUL zero — uninitialised tail on the wire is a
                // process-memory leak to whatever is listening.
                let name = &mut out[4..4 + CLIENT_NAME_MAX];
                name.fill(0);
                name[..client_name.len()].copy_from_slice(client_name.as_bytes());
            }
            Self::MoveAbs { x, y } => {
                out[0..4].copy_from_slice(&x.to_le_bytes());
                out[4..8].copy_from_slice(&y.to_le_bytes());
            }
            Self::MoveRel { dx, dy } => {
                if dx == 0 && dy == 0 {
                    return Err(EncodeError::ZeroDelta);
                }
                out[0..4].copy_from_slice(&dx.to_le_bytes());
                out[4..8].copy_from_slice(&dy.to_le_bytes());
            }
            Self::Scroll { notches_v, notches_h } => {
                if notches_v == 0 && notches_h == 0 {
                    return Err(EncodeError::ZeroDelta);
                }
                if notches_v.abs() > 1000 || notches_h.abs() > 1000 {
                    return Err(EncodeError::ScrollRange);
                }
                out[0..4].copy_from_slice(&notches_v.to_le_bytes());
                out[4..8].copy_from_slice(&notches_h.to_le_bytes());
            }
            Self::Button { code, down } => {
                out[0..2].copy_from_slice(&(code as u16).to_le_bytes());
                out[2] = u8::from(down);
                out[3] = 0; // reserved, validated by the daemon, not ignored
            }
            Self::Batch(items) => {
                out[0..2].copy_from_slice(&(items.len() as u16).to_le_bytes());
                out[2..4].copy_from_slice(&0u16.to_le_bytes()); // reserved
                for (i, item) in items.iter().enumerate() {
                    item.encode_into(&mut out[4 + 12 * i..4 + 12 * (i + 1)]);
                }
            }
        }
        Ok(need)
    }
}

/// `WIRE.md` §3.5 — the name is a label, not a credential, but it still has
/// to be a name: non-empty, printable ASCII, and short enough to NUL-terminate
/// inside 32 bytes.
pub fn valid_client_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < CLIENT_NAME_MAX
        && name.bytes().all(|b| b.is_ascii_graphic())
}

/// `WIRE.md` §3.4 — the HELLO answer. A client MUST accept any response of at
/// least 24 bytes and ignore a tail it does not understand; demanding exactly
/// 32 breaks the growth rule for the next field that gets appended.
#[derive(Debug, Clone, Copy)]
pub struct HelloResponse {
    pub proto_selected: u16,
    pub device_caps: u16,
    pub abs_range_max: u32,
    pub opcode_bitmap: u64,
    pub daemon_version: u32,
    pub reconnect: Option<Reconnect>,
}

#[derive(Debug, Clone, Copy)]
pub struct Reconnect {
    pub mode: u8,
    pub max_tries: u8,
    pub base_ms: u16,
}

impl HelloResponse {
    pub const MIN_LEN: usize = 24;

    pub fn decode(body: &[u8]) -> Option<Self> {
        if body.len() < Self::MIN_LEN {
            return None;
        }
        let u16at = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
        let u32at = |o: usize| {
            u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]])
        };
        Some(Self {
            proto_selected: u16at(0),
            device_caps: u16at(2),
            abs_range_max: u32at(4),
            opcode_bitmap: u64::from_le_bytes([
                body[8], body[9], body[10], body[11],
                body[12], body[13], body[14], body[15],
            ]),
            daemon_version: u32at(16),
            // body[20..24] is `reserved`, MUST be zero — checked by the caller
            // so the canary is visible rather than swallowed here.
            reconnect: if body.len() >= 30 {
                Some(Reconnect {
                    mode: body[24],
                    max_tries: body[25],
                    base_ms: u16at(26),
                })
            } else {
                None
            },
        })
    }

    /// `WIRE.md` §3.4 — gate on the opcode bit, **never** on `daemon_version`.
    /// Feature-sniffing by version is how a protocol grows a compatibility
    /// matrix nobody can test.
    pub fn supports(&self, op: Op) -> bool {
        self.opcode_bitmap & (1u64 << (op as u16)) != 0
    }

    pub fn has_cap(&self, bit: u16) -> bool {
        self.device_caps & bit != 0
    }
}

pub const CAP_POINTER_ABS: u16 = 1;
pub const CAP_KEYBOARD: u16 = 2;
pub const CAP_POINTER_REL: u16 = 4;
pub const CAP_BUTTONS: u16 = 8;
