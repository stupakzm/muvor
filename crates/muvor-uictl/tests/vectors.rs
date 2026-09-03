//! Conformance against uictl's own `vectors.json` (`WIRE.md` §9).
//!
//! Needs no daemon, no display and no session bus — which is why it belongs
//! in CI from M1. The vectors are generated from `src/proto.h`, so a field
//! that moves in uictl's header fails here rather than in production.

mod json;

use json::{hex, Json};
use muvor_uictl::validate::{validate_frame, ConnState};
use muvor_uictl::wire::{
    BatchItem, Button, Header, HelloResponse, Op, Request, SourceTag, HEADER_LEN,
};
use muvor_uictl::ResultCode;

/// Vectors muvor deliberately does not encode, and why. Listed rather than
/// silently skipped: a suite that quietly ignores a third of its oracle is
/// not the coverage it appears to be.
const NOT_ENCODED: &[(&str, &str)] = &[
    ("R8", "KEY_TAP — muvor injects no keys (plan.md §3.3)"),
    ("R9", "KEY_SEQUENCE — as above"),
    ("R10", "KEY_DOWN as a standalone frame — only ever a BATCH sub-op here"),
    ("R11", "KEY_UP as a standalone frame — only ever a BATCH sub-op here"),
    ("R13", "CONFIRM_SUBSCRIBE — muvor must not take the confirm role"),
    ("R14", "CONFIRM_DECIDE — as above"),
];

fn vectors_json() -> (String, String) {
    let mut tried = Vec::new();
    let mut cands: Vec<String> = Vec::new();
    if let Some(p) = std::env::var_os("UICTL_VECTORS") {
        cands.push(p.to_string_lossy().into_owned());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let h = home.to_string_lossy();
        cands.push(format!("{h}/projects/uictl/vectors.json"));
    }
    cands.push("/usr/share/uictl/vectors.json".into());
    cands.push("/usr/local/share/uictl/vectors.json".into());
    cands.push("../uictl/vectors.json".into());
    for c in &cands {
        match std::fs::read_to_string(c) {
            Ok(s) => return (s, c.clone()),
            Err(e) => tried.push(format!("  {c}: {e}")),
        }
    }
    panic!(
        "vectors.json not found — this suite is the wire layer's only oracle.\n\
         Set UICTL_VECTORS, or install uictl's $datadir/uictl/vectors.json.\n\
         Tried:\n{}",
        tried.join("\n")
    );
}

fn load() -> Vec<Json> {
    let (text, path) = vectors_json();
    let doc = json::parse(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
    let v = doc.get("vectors").expect("no `vectors` key").arr().to_vec();
    assert!(!v.is_empty(), "{path} carries no vectors");
    v
}

fn by_id(vs: &[Json], id: &str) -> Json {
    vs.iter()
        .find(|v| v.get("id").map(Json::str) == Some(id))
        .unwrap_or_else(|| panic!("vector {id} missing"))
        .clone()
}

/// The encoder must reproduce each request vector **byte for byte**.
#[test]
fn requests_encode_byte_exact() {
    let vs = load();
    let mut buf = vec![0u8; HEADER_LEN + muvor_uictl::wire::MAX_PAYLOAD];
    let mut checked = 0;

    for v in vs.iter().filter(|v| v.get("kind").map(Json::str) == Some("request")) {
        let id = v.get("id").unwrap().str().to_string();
        if NOT_ENCODED.iter().any(|(x, _)| *x == id) {
            continue;
        }
        let want = hex(v.get("bytes").expect("no bytes").str());
        let h = Header::decode(&want).expect("vector shorter than a header");

        // §9.1: every request vector is stamped source_tag = SRC_CLI and
        // carries the generator's own seq. Reproduce both from the vector so
        // the test asserts the payload, not our counter.
        let tag = SourceTag(h.source_tag);
        let body = &want[HEADER_LEN..];

        let req: Request = match Op::from_u16(h.opcode).expect("unknown opcode") {
            Op::Hello => {
                let name_end = body[4..].iter().position(|&b| b == 0).unwrap() + 4;
                Request::Hello {
                    proto_min: u16::from_le_bytes([body[0], body[1]]),
                    proto_max: u16::from_le_bytes([body[2], body[3]]),
                    client_name: std::str::from_utf8(&body[4..name_end]).unwrap(),
                }
            }
            Op::Ping => Request::Ping,
            Op::MoveAbs => Request::MoveAbs {
                x: i32::from_le_bytes(body[0..4].try_into().unwrap()),
                y: i32::from_le_bytes(body[4..8].try_into().unwrap()),
            },
            Op::MoveRel => Request::MoveRel {
                dx: i32::from_le_bytes(body[0..4].try_into().unwrap()),
                dy: i32::from_le_bytes(body[4..8].try_into().unwrap()),
            },
            Op::Scroll => Request::Scroll {
                notches_v: i32::from_le_bytes(body[0..4].try_into().unwrap()),
                notches_h: i32::from_le_bytes(body[4..8].try_into().unwrap()),
            },
            Op::Button => Request::Button {
                code: match u16::from_le_bytes([body[0], body[1]]) {
                    272 => Button::Left, 273 => Button::Right, 274 => Button::Middle,
                    275 => Button::Side, 276 => Button::Extra,
                    c => panic!("vector {id}: button code {c}"),
                },
                down: body[2] == 1,
            },
            Op::Batch => {
                let count = u16::from_le_bytes([body[0], body[1]]) as usize;
                let items: Vec<BatchItem> = (0..count)
                    .map(|i| {
                        let it = &body[4 + 12 * i..4 + 12 * (i + 1)];
                        let a = i32::from_le_bytes(it[4..8].try_into().unwrap());
                        let b = i32::from_le_bytes(it[8..12].try_into().unwrap());
                        match Op::from_u16(u16::from_le_bytes([it[0], it[1]])).unwrap() {
                            Op::MoveAbs => BatchItem::MoveAbs { x: a, y: b },
                            Op::MoveRel => BatchItem::MoveRel { dx: a, dy: b },
                            Op::Scroll => BatchItem::Scroll { notches_v: a, notches_h: b },
                            Op::Button => BatchItem::Button {
                                code: match a {
                                    272 => Button::Left, 273 => Button::Right,
                                    274 => Button::Middle, 275 => Button::Side,
                                    _ => Button::Extra,
                                },
                                down: b == 1,
                            },
                            Op::KeyDown => BatchItem::KeyDown { keycode: a as u16 },
                            Op::KeyUp => BatchItem::KeyUp { keycode: a as u16 },
                            o => panic!("vector {id}: {o:?} is not batchable"),
                        }
                    })
                    .collect();
                // Borrowed by the Request, so it must outlive the encode call.
                let n = Request::Batch(&items).encode_into(&mut buf, tag, h.seq).unwrap();
                assert_eq!(&buf[..n], &want[..], "vector {id} (BATCH) bytes differ");
                checked += 1;
                continue;
            }
            o => panic!("vector {id}: unexpected opcode {o:?}"),
        };

        let n = req.encode_into(&mut buf, tag, h.seq).unwrap();
        assert_eq!(
            &buf[..n], &want[..],
            "vector {id} ({}) bytes differ\n got {:02x?}\nwant {:02x?}",
            v.get("title").map(Json::str).unwrap_or(""), &buf[..n], want
        );
        checked += 1;
    }
    assert!(checked >= 7, "only {checked} request vectors exercised");
    eprintln!("encoded {checked} request vectors byte-exact");
}

/// R4 is `dx = -5`. It is the one vector that catches a sign-and-magnitude or
/// byte-swap bug in the encoder, and nothing else does — so it gets its own
/// test rather than being one loop iteration that could be skipped.
#[test]
fn r4_negative_delta_is_two_complement_little_endian() {
    let vs = load();
    let v = by_id(&vs, "R4");
    let want = hex(v.get("bytes").unwrap().str());
    let h = Header::decode(&want).unwrap();
    let body = &want[HEADER_LEN..];
    let dx = i32::from_le_bytes(body[0..4].try_into().unwrap());
    let dy = i32::from_le_bytes(body[4..8].try_into().unwrap());
    assert!(dx < 0 || dy < 0, "R4 is meant to carry a negative delta");

    let mut buf = vec![0u8; 64];
    let n = Request::MoveRel { dx, dy }
        .encode_into(&mut buf, SourceTag(h.source_tag), h.seq)
        .unwrap();
    assert_eq!(&buf[..n], &want[..], "R4 mismatch: sign or byte order is wrong");
}

/// `WIRE.md` §9: for responses, decode and assert every field **except** the
/// byte ranges the vector marks as `varies`.
#[test]
fn hello_response_decodes_ignoring_varies() {
    let vs = load();
    let s2 = by_id(&vs, "S2");
    let bytes = hex(s2.get("bytes").unwrap().str());
    let h = Header::decode(&bytes).unwrap();
    assert_eq!(h.opcode, Op::Hello as u16);

    let body = &bytes[HEADER_LEN..];
    assert_eq!(u16::from_le_bytes([body[0], body[1]]), ResultCode::OK.0);
    let payload = &body[2..];
    assert!(payload.len() >= HelloResponse::MIN_LEN);

    let varies: Vec<(usize, usize)> = s2
        .get("varies")
        .map(|v| {
            v.arr().iter()
                .map(|r| (r.get("offset").unwrap().usize(), r.get("len").unwrap().usize()))
                .collect()
        })
        .unwrap_or_default();
    assert!(!varies.is_empty(), "S2 should mark device-dependent fields");

    let r = HelloResponse::decode(payload).expect("HELLO body did not decode");
    assert_eq!(r.proto_selected, 1);
    // abs_range_max is NOT in `varies`: it is the coordinate contract, so it
    // is asserted. INT16_MAX, per uinput.h.
    assert_eq!(r.abs_range_max, 32767, "abs_range_max is the coordinate contract");
    // `reserved` is the canary of §3.4 against uninitialised tail padding.
    assert_eq!(&payload[20..24], &[0, 0, 0, 0], "reserved must be zero");

    // A client must accept the 24-byte floor and ignore the tail.
    assert!(HelloResponse::decode(&payload[..HelloResponse::MIN_LEN]).is_some());
    assert!(HelloResponse::decode(&payload[..HelloResponse::MIN_LEN - 1]).is_none());
    eprintln!("S2 varies ranges honoured: {varies:?}");
}

/// `WIRE.md` §9: a `reject` vector must produce exactly `expect_result`.
#[test]
fn rejected_frames_produce_expected_result() {
    let vs = load();
    let mut checked = 0;
    for v in vs.iter().filter(|v| v.get("kind").map(Json::str) == Some("reject")) {
        let id = v.get("id").unwrap().str();
        let bytes = hex(v.get("bytes").unwrap().str());
        let want = ResultCode(v.get("expect_result").unwrap().usize() as u16);

        // N5 is version hopping *after* the handshake, so it only rejects
        // against a connection that has pinned a version.
        let st = ConnState { pinned_version: Some(1) };
        let got = validate_frame(&bytes, st).expect_err(&format!("{id} was accepted"));
        assert_eq!(
            got, want,
            "vector {id} ({}): expected result {}, got {}",
            v.get("title").map(Json::str).unwrap_or(""), want.0, got.0
        );
        checked += 1;
    }
    assert_eq!(checked, 5, "expected 5 reject vectors");
}

/// The encoder must never produce a frame the daemon would refuse. This is
/// the mechanism behind the claim, not a comment asserting it.
#[test]
fn encoder_never_emits_a_frame_the_validator_rejects() {
    let mut buf = vec![0u8; HEADER_LEN + muvor_uictl::wire::MAX_PAYLOAD];
    let st = ConnState { pinned_version: Some(1) };
    let cases: Vec<Request> = vec![
        Request::Ping,
        Request::Hello { proto_min: 1, proto_max: 1, client_name: muvor_uictl::CLIENT_NAME },
        Request::MoveAbs { x: 0, y: 0 },
        Request::MoveAbs { x: 32767, y: 32767 },
        Request::MoveAbs { x: -1, y: 99999 }, // §5A.1: clamped, never refused
        Request::MoveRel { dx: -5, dy: 3 },
        Request::Scroll { notches_v: -1000, notches_h: 1000 },
        Request::Button { code: Button::Left, down: true },
        Request::Button { code: Button::Extra, down: false },
    ];
    for req in &cases {
        let n = req.encode_into(&mut buf, SourceTag::HOTKEY, 7).unwrap();
        validate_frame(&buf[..n], st)
            .unwrap_or_else(|e| panic!("{:?} produced a frame refused with {}", req.opcode(), e.0));
    }

    // The batch a hint jump actually sends — one token, not three.
    let items = [
        BatchItem::MoveAbs { x: 100, y: 200 },
        BatchItem::Button { code: Button::Left, down: true },
        BatchItem::Button { code: Button::Left, down: false },
    ];
    let n = Request::Batch(&items).encode_into(&mut buf, SourceTag::HOTKEY, 8).unwrap();
    validate_frame(&buf[..n], st).expect("hint-jump batch refused");
}

/// Negative control: if the validator cannot fail, the test above proves
/// nothing. Build the broken frames on purpose and watch it reject them.
#[test]
fn validator_actually_rejects() {
    let st = ConnState { pinned_version: Some(1) };
    let mut buf = vec![0u8; 128];
    let n = Request::Button { code: Button::Left, down: true }
        .encode_into(&mut buf, SourceTag::HOTKEY, 1).unwrap();

    let mut bad = buf[..n].to_vec();
    bad[HEADER_LEN + 3] = 1; // reserved != 0
    assert_eq!(validate_frame(&bad, st), Err(ResultCode::ERR_PAYLOAD_INVALID));

    let mut bad = buf[..n].to_vec();
    // BTN_LEFT is 272 = 0x0110, so clobbering the *low* byte with 0x10 is a
    // no-op — which is exactly how this control silently passed once. Clear
    // the high byte to get keycode 16: valid for §5B, never for §5A.
    bad[HEADER_LEN + 1] = 0x00;
    assert_eq!(u16::from_le_bytes([bad[HEADER_LEN], bad[HEADER_LEN + 1]]), 16);
    assert_eq!(validate_frame(&bad, st), Err(ResultCode::ERR_PAYLOAD_INVALID));

    let mut bad = buf[..n].to_vec();
    bad[12..16].copy_from_slice(&(muvor_uictl::wire::MAX_PAYLOAD as u32 + 1).to_le_bytes());
    assert_eq!(validate_frame(&bad, st), Err(ResultCode::ERR_TOO_LARGE));

    let mut bad = buf[..n].to_vec();
    bad[0..2].copy_from_slice(&9u16.to_le_bytes()); // version hop
    assert_eq!(validate_frame(&bad, st), Err(ResultCode::ERR_VERSION_UNSUPPORTED));
}

/// Locally-refused requests: the daemon would refuse these, so muvor never
/// sends them and pays no round trip to find out.
#[test]
fn encoder_refuses_what_the_daemon_would() {
    use muvor_uictl::EncodeError;
    let mut buf = vec![0u8; 512];
    let mut e = |r: Request| r.encode_into(&mut buf, SourceTag::HOTKEY, 1).unwrap_err();

    assert_eq!(e(Request::MoveRel { dx: 0, dy: 0 }), EncodeError::ZeroDelta);
    assert_eq!(e(Request::Scroll { notches_v: 0, notches_h: 0 }), EncodeError::ZeroDelta);
    assert_eq!(e(Request::Scroll { notches_v: 1001, notches_h: 0 }), EncodeError::ScrollRange);
    assert_eq!(e(Request::Batch(&[])), EncodeError::BatchSize);
    let too_many = [BatchItem::MoveRel { dx: 1, dy: 1 }; 17];
    assert_eq!(e(Request::Batch(&too_many)), EncodeError::BatchSize);
    assert_eq!(
        e(Request::Hello { proto_min: 1, proto_max: 1, client_name: "" }),
        EncodeError::BadClientName
    );
    assert_eq!(
        e(Request::Hello { proto_min: 1, proto_max: 1, client_name: "has space" }),
        EncodeError::BadClientName
    );
}

#[test]
fn vectors_we_do_not_encode_are_declared() {
    let vs = load();
    for (id, why) in NOT_ENCODED {
        let v = by_id(&vs, id);
        eprintln!("skipped {id} ({}): {why}", v.get("title").map(Json::str).unwrap_or(""));
    }
}
