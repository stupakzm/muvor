# muvor

Keyboard-driven pointer control for Linux, built for Wayland rather than
retrofitted onto it. Label every clickable thing on screen, type the label,
the pointer goes there.

> **Status:** in development. The architecture below is built; the product
> is not finished.

---

## The problem

Wayland deliberately denies clients two things X11 gave away freely: the
ability to read the global input stream, and the ability to know where
anything is on screen. That is a security win and a serious obstacle for
any pointer-control tool. The usual workarounds are to run the whole thing
as root, or to give the binary membership in the `input` group.

Both are worse than they look. `/dev/uinput` is `0660 root:input`, and
Linux permissions are UID + GID + mode — so the kernel cannot gate that
device per-binary. **Any process running as a user in the `input` group can
inject input and read `/dev/input/event*`.** A pointer-control tool that
asks for that permission is asking to be trusted as a keylogger, and it has
no way to prove it isn't one.

muvor doesn't ask.

## How it's put together

Three crates and a GNOME Shell extension, with the privileges pushed to one
end and the logic kept at the other.

```
  extension/          GNOME Shell (JS) — overlay, hotkey grab, screen capture
        │                                zbus
        ▼
  src/                the daemon binary — the only place a bus is touched
        │
        ├── muvor-atspi   reads the accessibility tree → targets + provenance
        ├── muvor-core    pure: what a target is called, what typing does
        └── muvor-uictl   client for the uictl broker → all movement, all clicks
                              │
                              ▼  AF_UNIX
                          uictld    holds /dev/uinput, alone
```

**`muvor-core`** never talks to a bus, a socket or a compositor. It is given
positions and returns labels. Its tests prove their claims outright instead
of measuring them.

**`muvor-atspi`** answers "where are the buttons" and stops there — it never
draws and never clicks. Every rectangle it produces is window-relative,
because AT-SPI on Wayland reports identical numbers for `SCREEN` and
`WINDOW`: a Wayland client is never told its own position. Screen
coordinates don't exist until the extension supplies the window origin.

**`muvor-uictl`** has no dependencies and needs no display, session bus or
running daemon to test.

The whole workspace is `unsafe_code = "forbid"`.

## The uictl layer

Every pointer movement and every click in muvor goes through
[**uictl**](https://github.com/stupakzm/uictl), a typed-RPC broker for
`/dev/uinput` written for exactly this problem.

`uictld` holds the kernel file descriptor and nothing else does. Clients ask
it to move the pointer or press a key over a unix socket, and it decides
whether they may — rate-limited by client class, checked against a static
destructive-key deny-list and a per-user allowlist, written to an audit log,
and for flagged clients, answered by a human.

This means muvor **never opens `/dev/uinput` and never reads
`/dev/input/event*`.** It doesn't need `input` group membership. It cannot
read your keystrokes, because it was never given the capability to — not as
a policy, as an architectural fact. Every click it makes is a request that
was authorised, recorded, and attributable.

The wire protocol is specified in `WIRE.md` in the uictl checkout, which is
normative for this crate; section numbers in `crates/muvor-uictl` refer to
it. `crates/muvor-uictl/tests/vectors.rs` runs against uictl's own
conformance vectors, so the two projects cannot silently drift apart.

For development without the real broker, `tools/fake-uictld.py` stands in.

## Targeting

Two modes:

- **Grid** — a static `asdfghjkl;` grid over the screen, refined by a
  subgrid. Simple to build, more keystrokes to arrive.
- **Detection** — read the accessibility tree, label only the things that
  are actually clickable, assign the shortest labels to the nearest targets.
  Harder, and far fewer keystrokes.

Detection is the one worth having, and the one the architecture is built
around.

## Requirements

- Linux with GNOME on Wayland
- [uictl](https://github.com/stupakzm/uictl), running as `uictld`
- Rust (edition and MSRV pinned in `Cargo.toml`)
- An AT-SPI-exposing toolkit for detection to see anything

## Build

```bash
cargo build --release
```

Install the extension:

```bash
extension/install.sh
```

Service units for `muvor-daemon` and `uictld` are in
[`packaging/systemd/`](packaging/systemd/). Adjust the `ExecStart` path in
`uictld.service` to wherever your uictl build lives.

## Repository layout

| Path | What's in it |
|---|---|
| `src/` | daemon binary — IPC, framing, geometry, shell boundary |
| `crates/muvor-core/` | pure logic: labelling, free mode, detection policy |
| `crates/muvor-atspi/` | accessibility-tree reading |
| `crates/muvor-uictl/` | uictl client — wire format and validation |
| `extension/` | GNOME Shell extension (overlay, grab, capture) |
| `tools/` | `fake-uictld.py`, playtest harness, benchmarks, error scanner |
| `measurements/` | latency and detection numbers |
| `packaging/` | systemd units |

Source comments cite `plan.md` and `ERRORS.md` by section — the design
document and the failure log. Both are kept outside the public repository.
The citations stay because that numbering is how the code is organised, not
because the files are missing by accident.

## Related

- [**uictl**](https://github.com/stupakzm/uictl) — the input broker muvor
  depends on. Built first, and the reason muvor can be safe.

## License

See [`Cargo.toml`](Cargo.toml).
