//! A mirror of the daemon's frame checks (`WIRE.md` §2, §3.1, §5A).
//!
//! muvor is a client, so this is not on the path to anything — it exists so
//! that the §9 `reject` vectors have something to run against, and so that
//! "our encoder never emits a frame the daemon would refuse" is a property a
//! test can assert rather than a claim a comment makes (doctrine: *a comment
//! is not a mechanism*).

use crate::wire::{Header, Op, ResultCode, CLIENT_NAME_MAX, HEADER_LEN, MAX_PAYLOAD};

/// `WIRE.md` §5B.1 — the kernel's `KEY_MAX`.
const KEY_MAX: u16 = 767;

/// What a daemon holds across one connection. `None` before the handshake.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConnState {
    pub pinned_version: Option<u16>,
}

/// Returns `Ok(())` for a frame a conforming daemon accepts, or the result
/// code it must answer with.
pub fn validate_frame(frame: &[u8], st: ConnState) -> Result<(), ResultCode> {
    let Some(h) = Header::decode(frame) else {
        return Err(ResultCode::ERR_PAYLOAD_INVALID);
    };

    // WIRE.md §2.1: payload_len is bounded *before* it is used as a length,
    // and before anything else about the frame is considered except version.
    if h.payload_len as usize > MAX_PAYLOAD {
        return Err(ResultCode::ERR_TOO_LARGE);
    }

    // WIRE.md §3.3 / §2.2: once a version is pinned, a frame stamped with a
    // different one is refused. HELLO is exempt — it is the bootstrap frame
    // and is accepted at any version (§3.1).
    let op = Op::from_u16(h.opcode);
    let is_hello = op == Some(Op::Hello);
    if let Some(pinned) = st.pinned_version {
        if !is_hello && h.version != pinned {
            return Err(ResultCode::ERR_VERSION_UNSUPPORTED);
        }
    }

    let Some(op) = op else {
        return Err(ResultCode::ERR_OPCODE_UNKNOWN);
    };

    let body = frame.get(HEADER_LEN..).unwrap_or(&[]);
    let declared = h.payload_len as usize;
    // The frame must actually carry what it declared. A short read is a
    // truncated frame, not a short payload.
    if body.len() < declared {
        return Err(ResultCode::ERR_PAYLOAD_INVALID);
    }
    let body = &body[..declared];

    // WIRE.md §2.3: command payloads are exact-size, never "at least".
    // HELLO is the one `>=`, for the reason §3.1 gives.
    let exact = |n: usize| if declared == n { Ok(()) } else { Err(ResultCode::ERR_PAYLOAD_INVALID) };

    match op {
        Op::Ping | Op::ConfirmSubscribe => exact(0)?,
        Op::Hello => {
            if declared < 4 + CLIENT_NAME_MAX {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            let min = u16::from_le_bytes([body[0], body[1]]);
            let max = u16::from_le_bytes([body[2], body[3]]);
            if min > max {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            if h.version < min || h.version > max {
                // §3.2 step 4: the frame is self-describing and must not
                // contradict itself.
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            let name = &body[4..4 + CLIENT_NAME_MAX];
            let Some(nul) = name.iter().position(|&b| b == 0) else {
                return Err(ResultCode::ERR_PAYLOAD_INVALID); // never terminated
            };
            if nul == 0 || !name[..nul].iter().all(u8::is_ascii_graphic) {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            if name[nul..].iter().any(|&b| b != 0) {
                return Err(ResultCode::ERR_PAYLOAD_INVALID); // junk after NUL
            }
        }
        Op::MoveAbs => exact(8)?, // §5A.1: any coordinate is valid; out of range clamps
        Op::MoveRel => {
            exact(8)?;
            let dx = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let dy = i32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            if dx == 0 && dy == 0 {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
        }
        Op::Scroll => {
            exact(8)?;
            let v = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let hh = i32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            if (v == 0 && hh == 0) || v.abs() > 1000 || hh.abs() > 1000 {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
        }
        Op::Button => {
            exact(4)?;
            let code = u16::from_le_bytes([body[0], body[1]]);
            // §5A.4: reserved is validated, not ignored, so it stays available
            // for a future field instead of filling with client junk.
            if body[3] != 0 || body[2] > 1 || !(272..=276).contains(&code) {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
        }
        Op::KeyDown | Op::KeyUp | Op::KeyTap => {
            exact(2)?;
            // §5B.1: keycodes are 1..=KEY_MAX. A range check, not policy —
            // the type bounds what is expressible, the daemon bounds what is
            // acceptable.
            let code = u16::from_le_bytes([body[0], body[1]]);
            if !(1..=KEY_MAX).contains(&code) {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
        }
        Op::Batch => {
            if declared < 4 {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            let count = u16::from_le_bytes([body[0], body[1]]) as usize;
            let reserved = u16::from_le_bytes([body[2], body[3]]);
            if reserved != 0 || count == 0 || count > 16 || declared != 4 + 12 * count {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            for i in 0..count {
                let it = &body[4 + 12 * i..4 + 12 * (i + 1)];
                let sub = u16::from_le_bytes([it[0], it[1]]);
                let sub_reserved = u16::from_le_bytes([it[2], it[3]]);
                if sub_reserved != 0 {
                    return Err(ResultCode::ERR_PAYLOAD_INVALID);
                }
                // §5B.4: exactly six sub-opcodes are batchable.
                match Op::from_u16(sub) {
                    Some(Op::MoveAbs | Op::MoveRel | Op::Scroll | Op::Button)
                    | Some(Op::KeyDown | Op::KeyUp) => {}
                    _ => return Err(ResultCode::ERR_PAYLOAD_INVALID),
                }
            }
        }
        // muvor encodes no key frames, but §9's N3 is a KEY_SEQUENCE and the
        // vectors are the oracle: an opcode left unchecked here is a hole in
        // the only thing testing this file.
        Op::KeySequence => {
            if declared < 4 {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            let count = u16::from_le_bytes([body[0], body[1]]) as usize;
            let reserved = u16::from_le_bytes([body[2], body[3]]);
            if reserved != 0 || count == 0 || count > 16 || declared != 4 + 4 * count {
                return Err(ResultCode::ERR_PAYLOAD_INVALID);
            }
            // §5B.2: self-balancing. Every press has its matching release
            // inside the same request, tracked item by item rather than
            // counted at the end — so a release-before-press fails here.
            let mut held: Vec<u16> = Vec::new();
            for i in 0..count {
                let it = &body[4 + 4 * i..4 + 4 * (i + 1)];
                let code = u16::from_le_bytes([it[0], it[1]]);
                let value = it[2];
                if it[3] != 0 || value > 1 || !(1..=KEY_MAX).contains(&code) {
                    return Err(ResultCode::ERR_PAYLOAD_INVALID);
                }
                if value == 1 {
                    if held.contains(&code) {
                        return Err(ResultCode::ERR_PAYLOAD_INVALID);
                    }
                    held.push(code);
                } else if let Some(pos) = held.iter().position(|&c| c == code) {
                    held.remove(pos);
                } else {
                    return Err(ResultCode::ERR_PAYLOAD_INVALID);
                }
            }
            if !held.is_empty() {
                return Err(ResultCode::ERR_PAYLOAD_INVALID); // unbalanced
            }
        }
        // The confirmer role, which muvor must never take (plan.md §3.3), and
        // which no reject vector exercises. Left unmodelled deliberately.
        Op::ConfirmDecide => {}
        Op::ConfirmRequest | Op::Invalid => return Err(ResultCode::ERR_OPCODE_UNKNOWN),
    }
    Ok(())
}
