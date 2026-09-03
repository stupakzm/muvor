/* muvor — the GNOME Shell half.
 *
 * This file holds **no product logic** (plan.md §5.5). It knows nothing
 * about accessibility, labels, geometry or uictl; it owns the four things
 * that are free inside the compositor and expensive or impossible outside
 * it — the key grab, the actors, the modal, and the window geometry a
 * Wayland client is never told (D13) — and it exposes them on D-Bus.
 *
 * GNOME breaks extension APIs on a real cadence (45 forced ESM; 46, 47 and
 * 48 each moved things). The mitigation is the size of this file, not
 * discipline about editing it: everything testable lives in Rust, so a
 * shell release can only ever break the part that has no tests anyway.
 *
 * At M4 the overlay draws whatever it is given, including the hardcoded
 * rectangles of `Demo()`. That is deliberate: when M5 fails, the question
 * "which half is broken" has already been answered.
 */

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

/* A modal grab holds the keyboard. If anything wedges while the overlay is
 * up — a bug here, a muvor process that died mid-session — the user is left
 * typing into nothing, and the fix would be a mouse trip to disable the
 * extension. So the overlay always has an end: it takes itself down after
 * this long, whatever else happens. */
const DEADMAN_SECONDS = 10;

/* How long the second key of a label must stay down before it means "hold
 * this one" rather than "click this one" (§12.3, movement mode).
 *
 * The tap path does NOT wait this long. A tap resolves on the key's own
 * release, which is 40-120 ms of human, so the added latency of an ordinary
 * click is the user's own finger and not this number. This is only the
 * deadline after which a key that has *not* come up is a hold. */
const HOLD_MS = 180;

/* How long after `_complete` the observer is given to report the press that
 * fired the accelerator.
 *
 * **This exists because handler order is not knowable from here.** mutter
 * dispatches an accelerator and clutter emits `captured-event`, and nothing
 * says which happens first — so at the instant a label completes, the press
 * that completed it may or may not have been recorded yet. Waiting a beat
 * costs 25 ms on a click and removes the guess. If no press has been seen by
 * then, this key's events are invisible here and the answer is a tap, which
 * is exactly v6's behaviour. */
const GRACE_MS = 25;

/* A press older than this cannot be the one that just completed a label. */
const SEEN_PRESS_MS = 150;

/* The fuse the M7 block asked for, in one line: `touch` this file and the
 * overlay goes back to `Main.pushModal` instead of §5.1e's per-key
 * accelerators.
 *
 * The reasoning is in the plan, and it is about what a wrong guess costs.
 * v6's grab is per-key, so if a modifier-less accelerator ever stops firing,
 * hints break *everywhere* — the overlay appears, typing does nothing, and
 * the deadman takes it down. Getting back to the exclusive grab that worked
 * everywhere except menus should cost a `touch`, not a third logout. */
const MODAL_FUSE = GLib.build_filenamev(
    [GLib.get_user_state_dir(), 'muvor', 'use-modal']);

/// The cyan dot's diameter. Small enough not to obscure what it marks, big
/// enough to find on a busy window.
const DOT = 10;

/* §5.3c's alphabet, as the compositor names it, plus the keys that must not
 * reach the application while the overlay is up (§5.1e).
 *
 * **`RANK_ROWS` in `muvor-core/src/label.rs` is normative and this mirrors
 * it.** A character muvor can put in a label and the shell cannot grab is a
 * badge that can never be typed — so the two lists are the same 60
 * characters, three rows and then the same three shifted, and the shifted
 * row really is `:<>?` rather than a repeat of `;,./`.
 *
 * The entries are *keysym names*, not the characters. `gtk_accelerator_parse`
 * refuses a literal ';' and takes 'semicolon'; the first probe fed it the
 * characters and had exactly `;,./` refused, which reads as "the compositor
 * will not grab punctuation" and is really "that is not what they are
 * called" (§5.1e). */
const LABEL_KEYS = (() => {
    const named = {';': 'semicolon', ',': 'comma', '.': 'period', '/': 'slash'};
    const bare = 'asdfghjkl;qwertyuiopzxcvbnm,./';
    const shifted = 'ASDFGHJKL:QWERTYUIOPZXCVBNM<>?';
    const out = [];
    for (let i = 0; i < bare.length; i++) {
        const key = named[bare[i]] ?? bare[i];
        out.push([key, bare[i]]);
        out.push([`<Shift>${key}`, shifted[i]]);
    }
    return out;
})();

/* Escape and Tab drive the overlay. The rest are grabbed for one reason: an
 * accelerator grab is per-key rather than exclusive, so anything not in this
 * list reaches the window underneath — and these are the keys that would
 * ACTIVATE or EDIT something there. They are swallowed and do nothing, which
 * is what the modal grab used to do for every key on the keyboard.
 *
 * What still leaks, stated rather than discovered: F1-F12, Home/End,
 * PgUp/PgDn, and modifier combinations. None of them activate anything on
 * their own. */
const CONTROL_KEYS = [
    'Escape', 'Tab', 'Return', 'KP_Enter', 'space',
    'Up', 'Down', 'Left', 'Right', 'BackSpace', 'Delete',
    /* v7: arms movement mode, so the next completed label goes *there*
     * instead of being clicked. It is the entry that cannot fail — holding
     * the second key is nicer and depends on seeing a key release, which
     * §12.3 could not promise from here. `apostrophe` and not the character,
     * for §5.1e's reason: that is what the parser calls it. */
    'apostrophe',
];

const BUS_NAME = 'org.muvor.Shell';
const OBJECT_PATH = '/org/muvor/Shell';

/* Six calls and two signals (§5.5). `Show` takes screen-space rectangles
 * because turning window-relative AT-SPI bounds into screen coordinates is
 * muvor's job once `FocusedWindow` has told it the origin — the shell does
 * no arithmetic it can avoid. */
const IFACE = `
<node>
  <interface name="org.muvor.Shell">
    <method name="Show">
      <arg type="a(iiiis)" direction="in" name="targets"/>
    </method>
    <method name="ShowMarked">
      <arg type="a(iiiiiis)" direction="in" name="targets"/>
    </method>
    <method name="Hide"/>
    <method name="FocusedWindow">
      <arg type="(siiiiisiiii)" direction="out" name="window"/>
    </method>
    <method name="Pointer">
      <arg type="(ii)" direction="out" name="position"/>
    </method>
    <method name="Demo"/>
    <method name="Capture">
      <arg type="ay" direction="out" name="png"/>
      <arg type="(iiii)" direction="out" name="area"/>
      <arg type="d" direction="out" name="scale"/>
    </method>
    <method name="GrabKeys"/>
    <method name="Probe">
      <arg type="(bbuu)" direction="out" name="observation"/>
    </method>
    <signal name="Typed">
      <arg type="s" name="label"/>
    </signal>
    <signal name="TypedHold">
      <arg type="s" name="label"/>
    </signal>
    <signal name="Cancelled"/>
    <signal name="Hotkey"/>
    <signal name="Deepen"/>
    <signal name="KeyDown">
      <arg type="s" name="name"/>
    </signal>
    <signal name="KeyUp">
      <arg type="s" name="name"/>
    </signal>
    <property name="Version" type="s" access="read"/>
  </interface>
</node>`;

/* Hardcoded rectangles for M4 (§7): a shape you can recognise at a glance,
 * so a misplaced overlay is obvious rather than plausible. */
function demoTargets() {
    const [sw, sh] = [global.stage.width, global.stage.height];
    const cols = 4;
    const rows = 3;
    const out = [];
    const alphabet = 'asdfghjkl;';
    for (let r = 0; r < rows; r++) {
        for (let c = 0; c < cols; c++) {
            const i = r * cols + c;
            const label = alphabet[Math.floor(i / alphabet.length)] + alphabet[i % alphabet.length];
            out.push([
                Math.round((sw / (cols + 1)) * (c + 1)) - 40,
                Math.round((sh / (rows + 1)) * (r + 1)) - 16,
                80,
                32,
                label,
            ]);
        }
    }
    return out;
}

export default class MuvorExtension extends Extension {
    enable() {
        this._targets = [];
        this._actors = [];
        this._typed = '';
        this._grab = null;          /* free mode's modal (§4.5c) */
        this._held = null;          /* the hint's accelerators (§5.1e) */
        this._accelId = null;
        this._overlay = null;
        this._deadman = null;
        this._armed = false;        /* the ' prefix (§12.3) */
        this._pending = null;       /* a completed label, waiting to know */
        this._watchKeys();

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameOwnerFlags.NONE, null, null, null);

        /* Straight to mutter: ~0.1 ms, no portal, no permission dialog, no
         * D-Bus on the hot path — which is what satisfies D9 (§5.2).
         *
         * `POPUP` is in the list so the hotkey still reaches us *while the
         * overlay is up*, which is what makes it a toggle (§5.1b) — a modal
         * grab takes the key events, but mutter still honours bindings whose
         * action mode matches the one the grab was pushed with. It is
         * deliberately not `SYSTEM_MODAL`: that mode belongs to somebody
         * else's password prompt, and drawing hints over one would be a
         * keyboard grab on top of a keyboard grab. */
        Main.wm.addKeybinding(
            'hint',
            this.getSettings(),
            Meta.KeyBindingFlags.NONE,
            Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW | Shell.ActionMode.POPUP,
            () => this._hotkey());
    }

    disable() {
        this._teardown();
        if (this._observerId) {
            global.stage.disconnect(this._observerId);
            this._observerId = null;
        }
        Main.wm.removeKeybinding('hint');
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = null;
        }
        if (this._dbus) {
            this._dbus.unexport();
            this._dbus = null;
        }
    }

    get Version() {
        return this.metadata.version?.toString() ?? '0.1.0';
    }

    /* ---- watching keys without taking them (v7, §12.3) ----------------
     *
     *  **Observation only.** This handler never returns `EVENT_STOP` and
     *  never changes what any key does; it exists to answer one question
     *  that the accelerator path cannot: *is that key still down?*
     *
     *  An accelerator reports activation. There is no release and therefore
     *  no "still held", which is why holding a label's second key could not
     *  be told from tapping it. A modal grab would give both — and would
     *  dismiss any open menu the instant it was pushed, which is the fault
     *  §5.1e exists to have fixed. So the grab stays per-key and this
     *  *watches*.
     *
     *  **Whether it can see anything at all is not assumed.** If mutter
     *  consumes an accelerator's press before the stage emits it, nothing
     *  here ever fires for the keys that matter, `_complete` sees no recent
     *  press, and the answer is a tap — v6's behaviour exactly. That is the
     *  whole safety argument: the feature degrades to the thing it replaced,
     *  per key, with no flag to set and no calibration to run. `Probe`
     *  reports what it has actually seen, so one command settles it after
     *  the logout rather than a debugging session.
     */
    _watchKeys() {
        this._seen = {presses: 0, releases: 0, lastPressAt: 0};
        this._observerId = global.stage.connect('captured-event', (_a, event) => {
            const type = event.type();
            if (type === Clutter.EventType.KEY_PRESS) {
                this._seen.presses++;
                this._seen.lastPressAt = GLib.get_monotonic_time() / 1000;
            } else if (type === Clutter.EventType.KEY_RELEASE) {
                this._seen.releases++;
                /* Any release while a label is pending is that label's key:
                 * the overlay holds every other key it cares about, and a
                 * stray one resolving this as a tap is v6's behaviour again,
                 * which is the direction it is safe to be wrong in. */
                if (this._pending)
                    this._resolve(false);
            }
            return Clutter.EVENT_PROPAGATE;
        });
    }

    /** What the observer has actually seen: whether presses and releases
     *  reach it at all, and how many. The answer to §12.3 in one call. */
    Probe() {
        return [
            this._seen.presses > 0,
            this._seen.releases > 0,
            this._seen.presses,
            this._seen.releases,
        ];
    }

    /* ---- D-Bus surface ------------------------------------------------ */

    /** Screen-space badge rectangles, each with the point muvor will click.
     *
     *  `Show`'s rectangles say where to *draw*; they stopped saying where to
     *  *click* when the badge moved off the target (§5.1a) and stopped being
     *  anywhere near it when badges started dodging each other (§5.1c). A
     *  badge above a button is no longer a claim about what the pointer will
     *  hit, so the point is sent explicitly and drawn — a cyan dot, exactly
     *  where the click lands.
     *
     *  A separate method rather than a wider `Show`, so a muvor talking to
     *  an older extension keeps working instead of failing on a signature it
     *  cannot know about until the session restarts (§5.4a).
     */
    ShowMarked(targets) {
        this.Show(
            targets.map(([bx, by, bw, bh, _cx, _cy, label]) => [bx, by, bw, bh, label]),
            targets.map(([, , , , cx, cy]) => [cx, cy]),
        );
    }

    /** Screen-space rectangles with their labels. */
    Show(targets, dots = null) {
        this._teardown();
        this._targets = targets;
        this._typed = '';
        if (!targets.length)
            return;

        this._overlay = new St.Widget({
            reactive: true,
            can_focus: true,
            track_hover: false,
            x: 0,
            y: 0,
            width: global.stage.width,
            height: global.stage.height,
        });
        /* addChrome, not a window: no surface, no map animation, no
         * stacking negotiation, no focus-stealing prevention (§5.1). */
        Main.layoutManager.addChrome(this._overlay, {trackFullscreen: true});

        // The dots first, so a badge is never hidden behind one.
        for (const [cx, cy] of dots ?? []) {
            const dot = new St.Widget({style_class: 'muvor-dot'});
            dot.set_position(cx - DOT / 2, cy - DOT / 2);
            dot.set_size(DOT, DOT);
            this._overlay.add_child(dot);
        }

        for (const [x, y, w, h, label] of targets) {
            const actor = new St.Label({style_class: 'muvor-label', text: label});
            /* Placed, never rendered from scratch on the hot path — at M4
             * the set is small enough that St does the shaping; §5.3's
             * pre-rasterised atlas arrives when M5 shows it is needed. */
            actor.set_position(x + Math.round(w / 2) - 14, y + Math.round(h / 2) - 11);
            this._overlay.add_child(actor);
            this._actors.push(actor);
        }

        /* **Not `Main.pushModal` — that closed the menus this exists to
         * label** (§5.1e, 2026-08-20). On Wayland an open menu is an
         * `xdg_popup` holding a grab; a compositor modal grab cancels it,
         * mutter sends `xdg_popup.popup_done`, and the client destroys the
         * popup. Photographed with no key pressed at all: LibreWolf's menu
         * gone, and twelve badges left floating in the column where its
         * items had been.
         *
         * So the keys are taken one at a time instead. `grab_accelerator`
         * routes a key to the shell *before* the client without taking a
         * compositor grab, so the popup keeps its own and survives. The
         * hotkey firing while a menu was open is what proved mutter still
         * dispatches accelerators under a client grab.
         *
         * If not one key could be taken, take the overlay down rather than
         * leave pixels on screen that swallow nothing — the same rule the
         * modal grab had. */
        /* THE FUSE (v7). v6's grab is per-key, so the failure mode of a
         * modifier-less accelerator that never *fires* is that hints break
         * everywhere: the overlay appears, typing does nothing, Escape does
         * nothing, and only the deadman ends it. That is a regression on the
         * working case, not just a failed fix — and getting back to v5's
         * exclusive grab should cost a `touch`, not a third logout.
         *
         *     touch ~/.local/state/muvor/use-modal
         *
         * The cost of the modal is known and documented: it dismisses menus
         * (§5.1e), which is the one case v6 exists for. */
        if (GLib.file_test(MODAL_FUSE, GLib.FileTest.EXISTS)) {
            console.log(`muvor: ${MODAL_FUSE} exists — modal grab instead of accelerators`);
            this._grab = Main.pushModal(this._overlay, {
                actionMode: Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW |
                    Shell.ActionMode.POPUP,
            });
            if (!this._grab) {
                this._teardown();
                this._emitCancelled();
                return;
            }
            this._keyId = this._overlay.connect('key-press-event', (_a, event) => {
                const symbol = event.get_key_symbol();
                if (symbol === Clutter.KEY_Escape) {
                    this._teardown();
                    this._emitCancelled();
                } else if (symbol === Clutter.KEY_Tab) {
                    this._emitBare('Deepen');
                } else if (symbol === Clutter.KEY_BackSpace) {
                    this._typed = this._typed.slice(0, -1);
                    this._repaint();
                } else {
                    const ch = String.fromCharCode(Clutter.keysym_to_unicode(symbol));
                    if (ch && ch !== '\0')
                        this._typeChar(ch);
                }
                return Clutter.EVENT_STOP;
            });
        } else {
            this._held = this._grabKeys();
            if (this._held.size === 0) {
                this._teardown();
                this._emitCancelled();
                return;
            }
            this._accelId = global.display.connect(
                'accelerator-activated', (_d, action) => this._onAccel(action));
        }

        this._deadman = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, DEADMAN_SECONDS, () => {
            this._deadman = null;
            this._teardown();
            this._emitCancelled();
            return GLib.SOURCE_REMOVE;
        });
    }

    Hide() {
        this._teardown();
    }

    /** The origin AT-SPI cannot know (D13, §4.2a), and the focus AT-SPI does
     *  not report (D14, §4.3a) — one call, because muvor needs both at the
     *  same instant or neither is worth having.
     *
     *  The pid and wm_class are identity, not decoration: muvor has to find
     *  this same window again in the accessibility tree, where windows are
     *  identified by title and titles are neither unique nor stable. The pid
     *  narrows it to one application in a single call on muvor's side
     *  (`GetConnectionUnixProcessID` against the a11y bus), and the title
     *  then picks a window within it. Matching on the title alone would
     *  aim the pointer at whichever window answered first. */
    FocusedWindow() {
        const win = global.display.focus_window;
        if (!win)
            return ['', 0, 0, 0, 0, 0, '', 0, 0, 0, 0];
        const rect = win.get_frame_rect();
        /* §5.1h: the frame rect is the window WITHOUT its client-side
         * shadow, and the accessibility frame is the *buffer*, shadow
         * included. On the About dialog that put the origin arithmetic 26 px
         * out — most of a 34x30 button — and §4.5 could not catch it,
         * because the claim is built and validated in a11y space and the
         * error is added afterwards. Report both and let muvor use the one
         * the a11y frame is actually measured in. Do NOT halve the size
         * difference instead: measured here the shadow is 26 px at the sides
         * and ~23 above, so a symmetric guess is wrong vertically. */
        const buf = win.get_buffer_rect();
        return [
            win.get_title() ?? '',
            rect.x, rect.y, rect.width, rect.height,
            win.get_pid(),
            win.get_wm_class() ?? '',
            buf.x, buf.y, buf.width, buf.height,
        ];
    }

    /** Where the pointer is, in stage coordinates.
     *
     *  The readback channel for §3.5's calibration, and as of 2026-08-18 the
     *  only one on this platform: AT-SPI emits no mouse events under mutter,
     *  measured, so nothing outside the compositor can see where an injected
     *  pointer actually landed. Synchronous and free — no round trip inside
     *  the shell, just the stage's own idea of the cursor. */
    Pointer() {
        const [x, y] = global.get_pointer();
        return [x, y];
    }

    /** M4's exit criterion, and the hotkey's action until M5 wires muvor in. */
    Demo() {
        this.Show(demoTargets());
    }

    /** The whole stage, as PNG bytes (§5.4, D16).
     *
     *  **PNG, and that is not a preference.** Measured 2026-08-20: all three
     *  raw-pixel routes out of the compositor are unusable from GJS, and two
     *  of them fail *silently*.
     *
     *    - `Clutter.Stage.paint_to_buffer` and `Cogl.Texture.get_data` take
     *      the destination as a caller-allocated `guint8*` array. GJS
     *      marshals a `Uint8Array` into that **by copy**, into a temporary it
     *      frees on return, so the compositor's writes land in memory nobody
     *      reads. The call returns success.
     *    - `Clutter.Stage.read_pixels` returns a transfer-full `guint8` array
     *      with no length, no fixed size and not zero-terminated, so GJS
     *      falls back to scanning for a NUL and truncates there — on pixel
     *      data, at the first black byte. It returns a plausible array.
     *
     *  `Shell.Screenshot` is the only correct route, and it encodes through
     *  `gdk_pixbuf_save_to_stream_async` — 38 ms for a flat frame, 61 ms for
     *  a desktop-like one, 151 ms for noise, at 1920x1080. That is four times
     *  the hint budget, so this is **not** on the hot path yet: it is what
     *  the region detector is built and tuned against, and what generates
     *  §5.4's labelled dataset. A native shim replaces it when a measurement
     *  says the detector has earned the hot path, and nothing above this
     *  layer changes when it does.
     *
     *  Async because `screenshot_area` is: GJS calls `CaptureAsync` in
     *  preference to `Capture` when it exists, and hands over the invocation
     *  to answer late.
     *
     *  No cursor — `screenshot_area` composites without one, which is what a
     *  detector wants. The overlay is not in the frame either, because muvor
     *  captures before it draws.
     */
    CaptureAsync(_params, invocation) {
        const stage = global.stage;
        const [w, h] = [stage.width, stage.height];
        const stream = Gio.MemoryOutputStream.new_resizable();
        const shooter = new Shell.Screenshot();

        shooter.screenshot_area(0, 0, w, h, stream, (source, result) => {
            try {
                const [, area] = source.screenshot_area_finish(result);
                stream.close(null);
                const bytes = stream.steal_as_bytes();

                /* From mutter, not from `St.ThemeContext.scale_factor`: the
                 * latter is an integer and cannot express the fractional
                 * scales GNOME 48 offers. muvor needs this to map a pixel in
                 * the returned image back to a coordinate it can click. */
                const display = global.display;
                const scale = display.get_monitor_scale(display.get_primary_monitor());

                invocation.return_value(GLib.Variant.new_tuple([
                    GLib.Variant.new_from_bytes(new GLib.VariantType('ay'), bytes, true),
                    new GLib.Variant('(iiii)', [area.x, area.y, area.width, area.height]),
                    GLib.Variant.new_double(scale),
                ]));
            } catch (e) {
                invocation.return_error_literal(
                    Gio.DBusError, Gio.DBusError.FAILED, `capture: ${e}`);
            }
        });
    }

    /* ---- the hotkey ----------------------------------------------------- */

    /** One key for both directions: it puts the overlay up, and it takes it
     *  down (§5.1b).
     *
     *  Dismissing is `Cancelled`, the same as Escape and the same as the
     *  deadman, so muvor learns about it through the one path it already
     *  handles — a hint session that ends without a label is a hint session
     *  that ends without a label, however it ended. The shell does not get an
     *  opinion about which of them happened.
     *
     *  Guarded on `_overlay` rather than on the action mode: the only
     *  overlay this may take down is the one this extension put up. It was
     *  guarded on `_grab` until §5.1e, which is no longer set for a hint —
     *  a hint holds accelerators now, not a modal — so that test silently
     *  became "is free mode running", and the toggle would have stopped
     *  working on the thing it exists for. */
    _hotkey() {
        if (this._overlay) {
            this._teardown();
            this._emitCancelled();
            return;
        }
        /* **This used to call `Demo()`**, which is why pressing the hotkey on
         * a real desktop drew M4's hardcoded rectangles instead of hinting
         * anything. That was correct at M4 — the point of `Demo` was to prove
         * the overlay without detection — and it silently stopped being
         * correct the moment M5 closed the loop, because the loop could only
         * ever be entered from a terminal.
         *
         * The shell still does not call *out* (§5.5). It says the key was
         * pressed and stops; muvor decides what a hint is, gathers it, and
         * calls `ShowMarked` like any other client. */
        this._emitBare('Hotkey');
    }

    /* ---- keyboard ------------------------------------------------------ */

    /* ---- the keys, taken one at a time (§5.1e) ------------------------ */

    /** Grab every key a label can contain, plus the ones that would do
     *  damage if they reached the window underneath.
     *
     *  Returns action-id -> what the key means. Measured in a nested shell
     *  before any of this was written: 71 accelerators, none refused,
     *  **1.13 ms to take and 0.80 ms to give back** — so this sits on the
     *  draw path without moving it.
     *
     *  A refusal is logged rather than fatal. §5.2a is the precedent: an
     *  accelerator can be contested, and it fails *silently*, so the one
     *  thing that must not happen is losing the whole overlay because one
     *  key of sixty was taken by an input method. The labels using a refused
     *  key are untypeable; every other label still works, and the journal
     *  says which. */
    _grabKeys() {
        const held = new Map();
        const refused = [];
        const take = (accel, meaning) => {
            const id = global.display.grab_accelerator(accel, Meta.KeyBindingFlags.NONE);
            if (!id || id === Meta.KeyBindingAction.NONE) {
                refused.push(accel);
                return;
            }
            /* Without this mutter files the binding under no action mode and
             * never dispatches it. */
            Main.wm.allowKeybinding(
                Meta.external_binding_name_for_action(id),
                Shell.ActionMode.ALL ?? Shell.ActionMode.NORMAL);
            held.set(id, meaning);
        };

        for (const [accel, ch] of LABEL_KEYS)
            take(accel, ch);
        for (const name of CONTROL_KEYS)
            take(name, name);

        if (refused.length)
            console.log(`muvor: ${refused.length} key(s) refused — ${refused.join(' ')}`);
        return held;
    }

    _releaseKeys() {
        if (!this._held)
            return;
        for (const id of this._held.keys()) {
            Main.wm.allowKeybinding(
                Meta.external_binding_name_for_action(id), Shell.ActionMode.NONE);
            global.display.ungrab_accelerator(id);
        }
        this._held = null;
    }

    /** One accelerator fired. The map says what it was, so unlike `_onKey`
     *  there is no keysym-to-unicode step: the character was decided when
     *  the key was grabbed. */
    _onAccel(action) {
        const key = this._held?.get(action);
        if (key === undefined)
            return;                       /* somebody else's binding */

        /* A label is completed and muvor is being told what it means; a key
         * arriving inside that window belongs to whatever comes next, not to
         * the label that is already decided. */
        if (this._pending)
            return;

        switch (key) {
        case 'apostrophe':
            /* Arm movement mode: the next completed label moves the pointer
             * there and stops, instead of clicking (§12.3). Pressing it
             * again disarms — the same argument as §5.1b's toggle, which is
             * that a mode you cannot leave with the key that entered it is a
             * mode that needs a mouse. */
            this._armed = !this._armed;
            console.log(`muvor: movement mode ${this._armed ? 'armed' : 'disarmed'}`);
            this._repaint();
            return;
        case 'Escape':
            this._teardown();
            this._emitCancelled();
            return;
        case 'Tab':
            /* D18 tier 2 on an overlay that is already up (§5.4g): you have
             * looked, and what you wanted is not labelled. The overlay stays
             * up and the keys stay held — muvor answers with `ShowMarked`. */
            this._emitBare('Deepen');
            return;
        case 'BackSpace':
            this._typed = this._typed.slice(0, -1);
            this._repaint();
            return;
        case 'Return':
        case 'KP_Enter':
        case 'space':
        case 'Up':
        case 'Down':
        case 'Left':
        case 'Right':
        case 'Delete':
            return;                       /* swallowed on purpose */
        }
        this._typeChar(key);
    }

    /** A label character. Unchanged from the modal-grab version — this is
     *  the part that decides when a label is complete, and it was correct. */
    _typeChar(ch) {
        const typed = this._typed + ch;

        /* The shell knows only string prefixes — never which label is
         * "valid", never what a target is. muvor decides what a completed
         * label means; this end decides only when a label is complete. */
        const matches = this._targets.filter(t => t[4].startsWith(typed));
        if (matches.length === 0) {
            /* A miss consumes nothing, so the next key is matched against
             * the prefix that really was typed. Never a click. */
            return;
        }

        this._typed = typed;
        const exact = matches.find(t => t[4] === typed);
        if (exact && matches.length === 1) {
            this._complete(exact[4]);
            return;
        }
        this._repaint();
    }

    /** A label is complete. The only question left is whether its last key
     *  was tapped or is being held (§12.3).
     *
     *  Four ways this ends and all of them end it:
     *    - the ' prefix was armed  -> movement mode, no waiting at all
     *    - a release is observed   -> a tap, on the user's own finger
     *    - GRACE_MS with no press observed -> this key is invisible here,
     *      so a hold cannot be told from a tap: a tap, which is v6
     *    - HOLD_MS with the key still down -> a hold
     */
    _complete(label) {
        if (this._armed) {
            this._armed = false;
            this._teardown();
            this._enterKeyGrab();
            this._emitTypedHold(label);
            return;
        }
        this._pending = {label, timer: null};
        this._pending.timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, GRACE_MS, () => {
            if (!this._pending)
                return GLib.SOURCE_REMOVE;
            this._pending.timer = null;
            const age = GLib.get_monotonic_time() / 1000 - this._seen.lastPressAt;
            if (this._seen.presses === 0 || age > SEEN_PRESS_MS) {
                /* Nothing is watching this key, so there is nothing to wait
                 * for. Click, exactly as v6 did. */
                this._resolve(false);
                return GLib.SOURCE_REMOVE;
            }
            this._pending.timer = GLib.timeout_add(
                GLib.PRIORITY_DEFAULT, HOLD_MS - GRACE_MS, () => {
                    if (this._pending)
                        this._pending.timer = null;
                    this._resolve(true);
                    return GLib.SOURCE_REMOVE;
                });
            return GLib.SOURCE_REMOVE;
        });
    }

    /** Settle a pending label. `held` decides which signal muvor gets, and
     *  a hold hands the keyboard straight over rather than dropping it and
     *  waiting to be asked — the key is still down, and a gap where nobody
     *  holds it is a release leaking into the application. */
    _resolve(held) {
        const pending = this._pending;
        if (!pending)
            return;
        this._pending = null;
        if (pending.timer)
            GLib.source_remove(pending.timer);
        this._teardown();
        if (held) {
            this._enterKeyGrab();
            this._emitTypedHold(pending.label);
        } else {
            this._emitTyped(pending.label);
        }
    }

    _repaint() {
        for (let i = 0; i < this._actors.length; i++) {
            const label = this._targets[i][4];
            const live = label.startsWith(this._typed);
            this._actors[i].remove_style_class_name('muvor-label-dim');
            if (!live)
                this._actors[i].add_style_class_name('muvor-label-dim');
        }
    }

    _teardown() {
        if (this._pending) {
            if (this._pending.timer)
                GLib.source_remove(this._pending.timer);
            this._pending = null;
        }
        this._armed = false;
        if (this._deadman) {
            GLib.source_remove(this._deadman);
            this._deadman = null;
        }
        /* The hint overlay holds accelerators (§5.1e); free mode still holds
         * a real modal, because it wants every key on the keyboard and is
         * never used over a menu. Both are released here, and releasing the
         * one that is not held is a no-op. */
        if (this._accelId) {
            global.display.disconnect(this._accelId);
            this._accelId = null;
        }
        this._releaseKeys();
        if (this._grab) {
            Main.popModal(this._grab);
            this._grab = null;
        }
        if (this._overlay) {
            if (this._keyId) {
                this._overlay.disconnect(this._keyId);
                this._keyId = null;
            }
            if (this._keyUpId) {
                this._overlay.disconnect(this._keyUpId);
                this._keyUpId = null;
            }
            Main.layoutManager.removeChrome(this._overlay);
            this._overlay.destroy();
            this._overlay = null;
        }
        this._actors = [];
        this._targets = [];
        this._typed = '';
    }

    _emitTyped(label) {
        /* M4's exit criterion is "typing a label prints it" — the print is
         * this line in the journal, and the signal is what M5 consumes. */
        console.log(`muvor: typed ${label}`);
        this._dbus?.emit_signal('Typed', new GLib.Variant('(s)', [label]));
    }

    _emitTypedHold(label) {
        console.log(`muvor: typed ${label} — held, movement mode`);
        this._dbus?.emit_signal('TypedHold', new GLib.Variant('(s)', [label]));
    }

    _emitCancelled() {
        console.log('muvor: cancelled');
        this._dbus?.emit_signal('Cancelled', null);
    }

    _emitBare(name) {
        console.log(`muvor: ${name.toLowerCase()}`);
        this._dbus?.emit_signal(name, null);
    }

    /* ---- free mode (D12, plan.md §4.5c) -------------------------------
     *
     *  A grab of its own, on purpose. Free mode wants every key, and the
     *  label overlay wants only the ones that spell a label — trying to
     *  serve both from `_onKey` would put the two most safety-relevant
     *  paths in the shell in the same branch. So this takes its own modal,
     *  forwards key *names* and nothing else, and shares only `_teardown`.
     *
     *  The shell still holds no product logic: it does not know what `i`
     *  means, that there is a pointer, or that anything moved. It reports
     *  keys. muvor decides that `ijkl` is a direction and space is a click
     *  (§4.5c), which is the same division `Typed` already draws.
     */
    GrabKeys() {
        this._enterKeyGrab();
    }

    /** Take the keyboard and forward every key by name, until Escape or the
     *  deadman. Both free mode (§4.5c) and movement mode (§12) are this and
     *  nothing else — the shell does not know which, and that is the point.
     *
     *  Called directly on the hold path, where dropping the keyboard between
     *  `_teardown` and muvor asking for it would leak a key release into the
     *  application underneath. */
    _enterKeyGrab() {
        /* `_overlay`, not `_grab`: since §5.1e a hint holds accelerators
         * rather than a modal, so testing `_grab` here would step straight
         * over a live hint overlay and leak both its actors and its keys. */
        if (this._overlay)
            this._teardown();

        this._overlay = new St.Widget({
            style_class: 'muvor-overlay',
            reactive: true,
            x: 0,
            y: 0,
            width: global.stage.width,
            height: global.stage.height,
        });
        Main.layoutManager.addChrome(this._overlay);

        this._grab = Main.pushModal(this._overlay, {
            actionMode: Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW | Shell.ActionMode.POPUP,
        });
        if (!this._grab) {
            this._teardown();
            this._emitCancelled();
            return;
        }
        this._keyId = this._overlay.connect('key-press-event', (_a, e) => this._onKeyEvent(e, true));
        this._keyUpId = this._overlay.connect('key-release-event', (_a, e) => this._onKeyEvent(e, false));

        /* Free mode is a *mode*: a user is in it until they leave, and 10 s
         * of thinking is not a wedge. But it is still a keyboard grab, so it
         * still ends by itself — six times the hint deadman, and the same
         * argument (§5.1b). */
        this._deadman = GLib.timeout_add_seconds(
            GLib.PRIORITY_DEFAULT, DEADMAN_SECONDS * 6, () => {
                this._deadman = null;
                this._teardown();
                this._emitCancelled();
                return GLib.SOURCE_REMOVE;
            });
    }

    /** One key, in either direction.
     *
     *  **Release matters as much as press here, which is new in v7.**
     *  Movement mode's `s` and `f` are *held* — `s` is a speed and `f` is
     *  the left mouse button itself — so a mode that only heard presses
     *  could start a drag and never end it. Escape is the one key this end
     *  interprets, and only because a grab that cannot be left from inside
     *  the grab is a bug rather than a mode.
     *
     *  Names, not characters: muvor's key tables want `h` and `Alt_R`, and a
     *  modifier-shifted or dead key has no useful character at all.
     *  `Clutter.keyval_name` is the one call here that a port (§6) would
     *  have to reimplement. */
    _onKeyEvent(event, down) {
        const symbol = event.get_key_symbol();
        if (down && symbol === Clutter.KEY_Escape) {
            this._teardown();
            this._emitCancelled();
            return Clutter.EVENT_STOP;
        }
        const name = Clutter.keyval_name(symbol);
        if (name) {
            this._dbus?.emit_signal(
                down ? 'KeyDown' : 'KeyUp', new GLib.Variant('(s)', [name]));
        }
        return Clutter.EVENT_STOP;
    }
}
