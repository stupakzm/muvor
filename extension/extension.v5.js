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

/// The cyan dot's diameter. Small enough not to obscure what it marks, big
/// enough to find on a busy window.
const DOT = 10;

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
      <arg type="(siiiiis)" direction="out" name="window"/>
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
    <method name="FreeMode"/>
    <signal name="Typed">
      <arg type="s" name="label"/>
    </signal>
    <signal name="Cancelled"/>
    <signal name="Hotkey"/>
    <signal name="Deepen"/>
    <signal name="Key">
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
        this._grab = null;
        this._overlay = null;
        this._deadman = null;

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

        /* An atomic exclusive grab inside the compositor: it cannot
         * half-fail, cannot race the map, and cannot lose to focus policy
         * (§5.1). If it is refused, take the overlay down rather than
         * leaving pixels on screen that swallow nothing. */
        this._grab = Main.pushModal(this._overlay, {
            actionMode: Shell.ActionMode.POPUP,
        });
        if (!this._grab) {
            this._teardown();
            this._emitCancelled();
            return;
        }
        this._keyId = this._overlay.connect('key-press-event', (_a, event) => this._onKey(event));
        global.stage.set_key_focus(this._overlay);

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
            return ['', 0, 0, 0, 0, 0, ''];
        const rect = win.get_frame_rect();
        return [
            win.get_title() ?? '',
            rect.x, rect.y, rect.width, rect.height,
            win.get_pid(),
            win.get_wm_class() ?? '',
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
        if (this._grab) {
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

    _onKey(event) {
        const symbol = event.get_key_symbol();
        if (symbol === Clutter.KEY_Escape) {
            this._teardown();
            this._emitCancelled();
            return Clutter.EVENT_STOP;
        }
        if (symbol === Clutter.KEY_BackSpace) {
            this._typed = this._typed.slice(0, -1);
            this._repaint();
            return Clutter.EVENT_STOP;
        }
        if (symbol === Clutter.KEY_Tab) {
            this._emitBare('Deepen');
            return Clutter.EVENT_STOP;
        }

        const unicode = Clutter.keysym_to_unicode(symbol);
        if (!unicode)
            return Clutter.EVENT_STOP;
        const typed = this._typed + String.fromCharCode(unicode);

        const matches = this._targets.filter(t => t[4].startsWith(typed));
        if (matches.length === 0)
            return Clutter.EVENT_STOP;

        this._typed = typed;
        const exact = matches.find(t => t[4] === typed);
        if (exact && matches.length === 1) {
            const label = exact[4];
            this._teardown();
            this._emitTyped(label);
            return Clutter.EVENT_STOP;
        }
        this._repaint();
        return Clutter.EVENT_STOP;
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
        if (this._deadman) {
            GLib.source_remove(this._deadman);
            this._deadman = null;
        }
        if (this._grab) {
            Main.popModal(this._grab);
            this._grab = null;
        }
        if (this._overlay) {
            if (this._keyId) {
                this._overlay.disconnect(this._keyId);
                this._keyId = null;
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
    FreeMode() {
        if (this._grab)
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
        this._keyId = this._overlay.connect('key-press-event', (_a, event) => this._onFreeKey(event));

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

    _onFreeKey(event) {
        const symbol = event.get_key_symbol();
        if (symbol === Clutter.KEY_Escape) {
            this._teardown();
            this._emitCancelled();
            return Clutter.EVENT_STOP;
        }
        /* Names, not characters: muvor's `Dir::from_key` wants `i`, and a
         * modifier-shifted or dead key has no useful character at all.
         * `Clutter.keyval_name` is the one thing here that would have to be
         * reimplemented on another compositor, and it is one call. */
        const name = Clutter.keyval_name(symbol);
        if (name)
            this._dbus?.emit_signal('Key', new GLib.Variant('(s)', [name]));
        return Clutter.EVENT_STOP;
    }
}
