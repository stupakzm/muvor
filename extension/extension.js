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

/* v7's GRACE_MS and SEEN_PRESS_MS are gone, and so is the guess they
 * managed. They existed because v7 tried to learn "is that key still down?"
 * by watching `global.stage`'s `captured-event`, and mutter's own source
 * says that can never work for a key it has grabbed:
 *
 *     // src/core/events.c — registered with clutter_event_add_filter(),
 *     // which runs BEFORE any stage signal is emitted
 *     if (!meta_compositor_get_current_window_drag (compositor) &&
 *         meta_keybindings_process_event (display, window, event))
 *       return CLUTTER_EVENT_STOP;
 *
 * A grabbed accelerator is consumed there, so no press and no release for a
 * label key ever reaches the stage. `_complete` therefore always found no
 * recent press and always clicked — every hold was a tap, which is what was
 * reported from the desk on 2026-08-25. §12.13 has the whole reading. */

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

/* The second fuse: back to v6/v7's per-key accelerators.
 *
 *     touch ~/.local/state/muvor/use-accels
 *
 * v8 takes neither a compositor grab nor 71 accelerators. It sets the
 * **clutter key focus** to the overlay, and the same file says why that is
 * the one door that was never tried:
 *
 *     static gboolean
 *     stage_has_key_focus (MetaDisplay *display)
 *     {
 *       return clutter_stage_get_key_focus (stage) == NULL;
 *     }
 *     ...
 *     // Do not pass keyboard events to Wayland if key focus is not on the
 *     // stage in normal mode (e.g. during keynav in the panel)
 *     if (!has_grab)
 *       {
 *         if (IS_KEY_EVENT (event_type) && !stage_has_key_focus (display))
 *           return CLUTTER_EVENT_PROPAGATE;
 *       }
 *
 * With key focus on an actor of ours, that `PROPAGATE` returns above
 * `meta_wayland_compositor_handle_event`, so:
 *
 *   - clutter delivers the event to the focused actor — **press AND
 *     release**, which is the whole of §12.3 and cost v7 a logout to not get
 *   - the application underneath is never given the key, which is what the
 *     modal grab was for
 *   - **no compositor grab is taken**, so `clutter_stage_get_grab_actor()`
 *     stays NULL, no `xdg_popup` is cancelled, and the menu survives — which
 *     is what the accelerators were for (§5.1e)
 *
 * It is the third option §5.1e looked for and did not find, and mutter's own
 * comment names the precedent: the shell's panel keynav already does this.
 *
 * WHAT IT COSTS. Key focus is exclusive while it is held, so the leak-through
 * §5.1e itemised (F1-F12, Home/End, PgUp/PgDn, modifier combinations) stops
 * leaking. Global keybindings still fire: mutter runs
 * `meta_keybindings_process_event` above the key-focus check, so Alt+; and
 * Super still work, and the deadman still ends the overlay whatever happens.
 *
 * **Key focus must always be given back.** An overlay destroyed without
 * `set_key_focus(null)` leaves a keyboard that types into nothing, and that
 * is worse than any bug this fixes — so `_teardown` restores it, `disable`
 * restores it, and the deadman guarantees `_teardown` runs. */
const ACCEL_FUSE = GLib.build_filenamev(
    [GLib.get_user_state_dir(), 'muvor', 'use-accels']);

/* The bit clutter sets on an autorepeat, `CLUTTER_EVENT_FLAG_REPEATED`
 * (§12.16). Movement mode reads a key going down that is already down as its
 * lost release, and this is the whole of what makes that safe rather than
 * catastrophic — without it a held `d` is a re-press thirty times a second.
 *
 * **Named from the constant, and then checked against it.** GJS builds an
 * enum member's name from its GIR nick, so this one is `FLAG_REPEATED` and
 * not `REPEATED` — and `x & undefined` is `0` in JavaScript, which turns a
 * one-word mistake into "no key is ever a repeat" with no error anywhere.
 * Measured 2026-08-27, with `REPEATED` written here: one playtest run of two
 * intended clicks produced **nine clicks and seven false repairs**. The
 * literal is the fallback and `enable` says so out loud, because a silently
 * disabled safety check is the shape of fault this file keeps paying for. */
const REPEAT_FLAG = Clutter.EventFlags.FLAG_REPEATED ?? 0x4;
const REPEAT_FLAG_NAMED = Clutter.EventFlags.FLAG_REPEATED !== undefined;

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
    <method name="Windows">
      <arg type="a(siiiiisiiiibb)" direction="out" name="windows"/>
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
    <!-- v10. The keyboard repeating a key that is still down, told apart
         from a second press of it by CLUTTER_EVENT_FLAG_REPEATED rather
         than guessed at from timing. It is still only a key going down; the
         signal exists so muvor can read a bare KeyDown for a key it already
         believes is held as the one other thing it can be - evidence that
         key's KeyUp was lost (plan.md 12.16). -->
    <signal name="KeyRepeat">
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
        this._btnId = null;
        this._focusOutId = null;
        this._refocusId = 0;
        this._retaking = false;     /* `_onKeyFocusOut`'s re-entrancy guard */
        this._overlay = null;
        this._deadman = null;
        this._armed = false;        /* the ' prefix (§12.3) */
        this._pending = null;       /* a completed label, waiting to know */
        this._state = 'idle';       /* 'idle' | 'labels' | 'keys' — see `_setState` */
        this._hasKeyFocus = false;
        this._seen = {presses: 0, releases: 0};

        if (!REPEAT_FLAG_NAMED) {
            console.log('muvor: Clutter.EventFlags.FLAG_REPEATED is gone from this ' +
                'GNOME — falling back to the literal 0x4. If autorepeat starts ' +
                'arriving as KeyDown, that constant moved (§12.16).');
        }

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
        /* Belt and braces. `_teardown` already restores it; a keyboard that
         * types into nothing is the one failure here that a user cannot
         * recover from without a mouse, so it is given back twice. */
        this._releaseKeyFocus();
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

    /* ---- watching keys, v8: by holding the key focus (§12.13) ---------
     *
     *  v7 watched `global.stage`'s `captured-event` and saw nothing that
     *  mattered, because mutter consumes a grabbed accelerator inside a
     *  `clutter_event_add_filter()` callback — above every stage signal.
     *  The constant block at the top of this file quotes the two pieces of
     *  `src/core/events.c` that say so, and the door that is open instead.
     *
     *  So v8 does not watch. It **takes the key focus**, which routes every
     *  key into clutter and to one actor of ours, press and release, without
     *  a compositor grab and therefore without cancelling anybody's popup.
     *  There is nothing left to probe for and no degraded mode to fall back
     *  to: if a key reaches this extension at all, its release does too.
     *
     *  `_seen` survives only so `Probe` keeps answering — it counts what the
     *  overlay itself handled, which is the number that was worth having.
     */
    _takeKeyFocus(actor) {
        global.stage.set_key_focus(actor);
        this._hasKeyFocus = true;
    }

    _releaseKeyFocus() {
        if (!this._hasKeyFocus)
            return;
        /* Unconditionally to null rather than to whatever was focused
         * before: nothing else in the shell held it, and a stale actor
         * reference here is a keyboard that types into a destroyed widget. */
        global.stage.set_key_focus(null);
        this._hasKeyFocus = false;
    }

    /** What the overlay has actually handled: presses, releases, and whether
     *  a release ever arrived while a label was pending — which is the one
     *  fact §12.3 ever wanted, and the one v7 could not get.
     *
     *  `muvor check` prints it. Unlike v7's version this counts events this
     *  extension *received*, not events it hoped to overhear, so a zero here
     *  means the input path is broken rather than merely unobservable. */
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
        this._setState('labels');

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

        /* **The keys, v8: by taking the clutter key focus.**
         *
         * The three ways this has been done, and why this is the third:
         *
         *   v5  `Main.pushModal` — every key, press and release, and it
         *       **cancelled every open menu** (§5.1e). A compositor grab
         *       ends an `xdg_popup`; photographed with no key pressed.
         *   v6  71 per-key accelerators — menus survive, because no grab is
         *       taken. But an accelerator is an *activation*: there is no
         *       release, so a hold cannot be told from a tap, and v7's
         *       attempt to overhear the release on the stage could not work
         *       (see the constant block at the top of this file).
         *   v8  `global.stage.set_key_focus(overlay)` — mutter stops handing
         *       keys to Wayland and clutter delivers them here instead,
         *       press *and* release, while `clutter_stage_get_grab_actor()`
         *       stays NULL so no popup is cancelled.
         *
         * Both older paths are still here behind a `touch`, because being
         * wrong about the input path is how hints break everywhere and the
         * way back should not cost a logout. */
        if (GLib.file_test(MODAL_FUSE, GLib.FileTest.EXISTS)) {
            console.log(`muvor: ${MODAL_FUSE} exists — v5's modal grab`);
            this._grab = Main.pushModal(this._overlay, {
                actionMode: Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW |
                    Shell.ActionMode.POPUP,
            });
            if (!this._grab) {
                this._teardown();
                this._emitCancelled();
                return;
            }
            this._connectKeys();
        } else if (GLib.file_test(ACCEL_FUSE, GLib.FileTest.EXISTS)) {
            console.log(`muvor: ${ACCEL_FUSE} exists — v6's accelerators, no hold gesture`);
            this._held = this._grabKeys();
            if (this._held.size === 0) {
                this._teardown();
                this._emitCancelled();
                return;
            }
            this._accelId = global.display.connect(
                'accelerator-activated', (_d, action) => this._onAccel(action));
        } else {
            this._takeKeyFocus(this._overlay);
            this._connectKeys();
        }

        this._armDeadman(DEADMAN_SECONDS);
    }

    Hide() {
        this._teardown();
    }

    /* ---- the overlay's three states (§12.15) ---------------------------
     *
     *   idle     nothing is up and the keyboard is the application's.
     *   labels   `Alt+;` drew badges. The overlay **is** the screen: full
     *            size, reactive, in the shell's input region, spelling a
     *            label out of §5.3c's alphabet.
     *   keys     a label resolved as a HOLD, or free mode was entered. The
     *            badges are done and the only job left is reporting
     *            `a s d f g r h j k l` by name, so the overlay keeps the key
     *            focus and gives up everything else.
     *
     *  **The state IS the input ownership, which is why it is one method.**
     *  Until 2026-08-27 entering `keys` dropped only the children: the
     *  widget stayed full-screen and reactive in the chrome layer for the
     *  whole of movement mode, so every button muvor injected was picked by
     *  muvor's own actor and no application ever saw a click. `hjkl` worked
     *  because pointer motion is not picked against an actor; `f d g a r`
     *  all end in a button and all died, in every window. Measured with a
     *  `button-press-event` probe on the overlay:
     *  `overlay got button-press 2 mode=keys`.
     *
     *  **It is not `hide()`, and not a destroy.** §12.12 measured that
     *  one: hiding the actor that holds the clutter key focus hands the
     *  focus back, and then every key of movement mode goes to the
     *  application instead. `_applyInputOwnership` has what does work, and
     *  the two cheaper things that do not. */
    _setState(next) {
        if (this._state === next)
            return;
        this._state = next;
        this._applyInputOwnership();
        /* The journal line is the mechanism, not a comment: `journalctl |
         * grep muvor` is the first step when a gesture does nothing (§12.12),
         * and this is what tells that step whether the screen was still
         * muvor's when the click was injected. */
        const o = this._overlay;
        console.log(`muvor: state ${next}` +
            (o ? ` — overlay ${o.width}x${o.height}, reactive ${o.reactive}` : ' — no overlay'));
    }

    /** Own the screen, or give it back. The only place either happens. */
    _applyInputOwnership() {
        const o = this._overlay;
        if (!o)
            return;
        if (this._state === 'labels')
            return;                     /* `Show` built it owning the screen */

        /* **Concealed: out from under the pointer, still the keyboard.**
         * Three levers were measured, 2026-08-27, and only the third does
         * both jobs at once:
         *
         *   `set_reactive(false)`
         *       **0 keys reach the overlay.** The actor that holds the
         *       clutter key focus has to stay reactive to be sent any, so
         *       this is not available at any price.
         *
         *   `addChrome(o, {affectsInputRegion: false})`
         *       keys stay perfect and **the button still arrives** —
         *       `STRAY BUTTON 2 ... in state keys`. On Wayland what decides
         *       where a button goes is clutter picking the topmost reactive
         *       actor under the pointer, not the shell's input region, and a
         *       full-screen reactive actor is always that.
         *
         *   a 1x1 actor parked off-screen
         *       nothing is ever picked at the pointer, and geometry is not
         *       something the key focus depends on. This one.
         *
         * The actor stays mapped throughout — only its allocation moves —
         * which is what keeps §12.12's fault out of this. */
        o.set_size(1, 1);
        o.set_position(-64, -64);
    }

    /** Arm the overlay's dead-man, replacing any that is already running.
     *
     *  **One place, because there are three callers and they used to be
     *  three copies.** The overlay always has an end: a muvor that dies
     *  mid-hint, a bug in this file, anything at all, and the keyboard still
     *  comes back without a mouse trip to disable the extension (§5.1b).
     *  `_teardown` gives the key focus back, so arming this is what makes
     *  that promise unconditional.
     *
     *  Re-armed on every key in `keys` state (`_onOverlayKey`): a mode is a
     *  place the user stands, and the ten-second argument is about a
     *  keyboard nobody is typing into. */
    _armDeadman(seconds) {
        if (this._deadman)
            GLib.source_remove(this._deadman);
        this._deadman = GLib.timeout_add_seconds(
            GLib.PRIORITY_DEFAULT, seconds, () => {
                this._deadman = null;
                this._teardown();
                this._emitCancelled();
                return GLib.SOURCE_REMOVE;
            });
    }

    /** Take the keyboard back from whatever muvor's own click just focused.
     *
     *  **A click is a focus change, and a focus change clears the stage key
     *  focus.** For as long as the overlay swallowed every click this could
     *  not be seen; the moment the clicks land, the *first* action key of a
     *  mode is the last key that mode ever hears. Measured 2026-08-27 in
     *  `tools/playtest.sh race`: `overlay LOST key focus in state keys`,
     *  470 ms in, immediately after the wheel click reached
     *  gnome-text-editor — with `mapped` unchanged, so the actor was never
     *  hidden and §12.12's fault is not this one.
     *
     *  In `keys` the keyboard is muvor's by definition, and Escape, Tab and
     *  the dead-man are the ways out — so taking it back is not a fight, it
     *  is the mode staying the mode.
     *
     *  **v9 re-asserted it from an idle and that was too slow** — 16.006 ms,
     *  measured, and every key inside that window is the application's. v10
     *  takes it back here, in the same dispatch, and keeps the idle only as
     *  a fallback (§12.16). */
    _onKeyFocusOut() {
        if (this._state !== 'keys' || !this._overlay || this._retaking)
            return;

        /* **Synchronously, in this dispatch.** The idle was measured on
         * 2026-08-27 and it is not fast enough: `key-focus-out` to the idle
         * running took **16.0 ms** in the sandbox, and every key that
         * arrives inside that window is delivered to the application
         * instead — which for movement mode is a release that never comes
         * back, and one action key dead for the rest of the mode (§12.16).
         * Taking it back here closes the window to nothing: measured on the
         * same run, `sync re-take -> focus is OURS`, and the *second*
         * focus-out of the run stopped happening at all.
         *
         * `_retaking` is the re-entrancy guard the idle used to be. Setting
         * the key focus emits `key-focus-out` on whatever held it, and if
         * that is ever us again this would recurse. */
        this._retaking = true;
        this._takeKeyFocus(this._overlay);
        this._retaking = false;

        /* And the idle stays, as a fallback rather than the mechanism: if
         * the synchronous take did not stick — mutter is mid-focus-change
         * and about to clear it again — one more attempt costs nothing and
         * the mode is not left deaf. */
        if (this._refocusId)
            return;
        this._refocusId = GLib.idle_add(GLib.PRIORITY_HIGH, () => {
            this._refocusId = 0;
            if (this._state === 'keys' && this._overlay &&
                global.stage.key_focus !== this._overlay)
                this._takeKeyFocus(this._overlay);
            return GLib.SOURCE_REMOVE;
        });
    }

    /** A button that reached the overlay is a click an application did not
     *  get. Dormant by construction once `_applyInputOwnership` has dropped
     *  `reactive` — which is exactly what makes it a regression check for
     *  the fault above rather than decoration. */
    _onStrayButton(event) {
        console.log(`muvor: STRAY BUTTON ${event.get_button()} reached the overlay ` +
            `in state ${this._state} — the input region was not given back (§12.15)`);
        return Clutter.EVENT_PROPAGATE;
    }

    /** Press and release on the one actor that holds the key focus.
     *
     *  Both directions, always — the release is not an extra, it is the
     *  thing v8 exists to have. Movement mode's `f` is the left button and
     *  `s` is a speed, both *held*, so a path that heard only presses could
     *  start a drag and never end it (§12). */
    _connectKeys() {
        this._keyId = this._overlay.connect(
            'key-press-event', (_a, e) => this._onOverlayKey(e, true));
        this._keyUpId = this._overlay.connect(
            'key-release-event', (_a, e) => this._onOverlayKey(e, false));
        this._btnId = this._overlay.connect(
            'button-press-event', (_a, e) => this._onStrayButton(e));
        this._focusOutId = this._overlay.connect(
            'key-focus-out', () => this._onKeyFocusOut());
    }

    /** One key, one direction, and which of the two jobs the overlay is
     *  doing decides what it means.
     *
     *  `labels` — spelling a label out of §5.3c's alphabet, plus the keys
     *  that drive the overlay itself. A release matters here for exactly one
     *  reason: it settles a completed label as a **tap** (§12.3).
     *
     *  `keys` — free mode and movement mode. Names, not characters, and the
     *  shell still does not know what any of them mean (§5.5, D5).
     */
    _onOverlayKey(event, down) {
        const symbol = event.get_key_symbol();

        /* **The keyboard repeating a key is not a second press of it, and
         * since v10 that is a fact rather than a guess.** Clutter sets
         * `CLUTTER_EVENT_FLAG_REPEATED` on every autorepeat; measured
         * 2026-08-27, one press of `l` held for 900 ms arrived as
         * `flags=0` once and `flags=4` seven times, at 40 ms apart.
         *
         * v6 through v9 threw the distinction away — every repeat went out
         * as an ordinary `KeyDown` — and both halves then had to defend
         * themselves against it by guessing: 436 presses to 45 releases
         * measured on the desk, `d`/`g`/`r` clicking thirty times a second
         * until `Motion` grew a per-key latch. Reporting it is what lets
         * muvor read a bare `KeyDown` for a key it already believes is held
         * as the only other thing it can be — that key's `KeyUp` was lost
         * (§12.16). */
        const repeat = down && (event.get_flags() & REPEAT_FLAG) !== 0;

        if (down) {
            if (!repeat)
                this._seen.presses++;
        } else {
            this._seen.releases++;
        }

        /* Escape is the one key both jobs interpret, and only because a
         * keyboard that cannot be given back from inside is a bug rather
         * than a mode. */
        if (down && !repeat && symbol === Clutter.KEY_Escape) {
            this._teardown();
            this._emitCancelled();
            return Clutter.EVENT_STOP;
        }

        if (this._state === 'keys') {
            /* **A mode being used is not a mode that has been abandoned.**
             * The dead-man is what guarantees the keyboard comes back if
             * muvor dies mid-mode (§5.1b), and until v10 it was armed once
             * on the way in and never touched again — so movement mode
             * ended sixty seconds after it started however hard the user
             * was working in it. A dead-man that fires while the hand is on
             * it is not a dead-man; it is a timer. */
            this._armDeadman(DEADMAN_SECONDS * 6);
            const name = Clutter.keyval_name(symbol);
            if (name) {
                this._dbus?.emit_signal(
                    down ? (repeat ? 'KeyRepeat' : 'KeyDown') : 'KeyUp',
                    new GLib.Variant('(s)', [name]));
            }
            return Clutter.EVENT_STOP;
        }

        /* **A held label key repeats, and none of what follows may run
         * twice for one press.** `'` toggles movement mode, so a repeat
         * flips it thirty times a second and leaves it wherever the last
         * one landed; Tab asks muvor for tier 2, which is a capture and a
         * re-detect per repeat. And the hold gesture is a key deliberately
         * held down (§12.3) — the state that produces the most repeats in
         * this file is the one state that must ignore them. Swallowed, not
         * propagated: the key focus is ours and the application must not
         * get it either. */
        if (repeat)
            return Clutter.EVENT_STOP;

        /* A release while a label is pending is that label's last key coming
         * up: nothing else can reach this actor, because it holds the key
         * focus. **This is the measurement v7 could not make.** */
        if (!down) {
            if (this._pending)
                this._resolve(false);
            return Clutter.EVENT_STOP;
        }

        if (symbol === Clutter.KEY_Tab) {
            this._emitBare('Deepen');
        } else if (symbol === Clutter.KEY_BackSpace) {
            this._typed = this._typed.slice(0, -1);
            this._repaint();
        } else if (symbol === Clutter.KEY_apostrophe) {
            /* §12.3's prefix: the next completed label goes to movement mode
             * instead of being clicked. It was the entry that could not fail
             * when holding a key could not be detected; it stays because it
             * is also the entry that needs no second hand. Pressing it again
             * disarms, for §5.1b's reason — a mode you cannot leave with the
             * key that entered it is a mode that needs a mouse. */
            this._armed = !this._armed;
            console.log(`muvor: movement mode ${this._armed ? 'armed' : 'disarmed'}`);
            this._repaint();
        } else if (symbol === Clutter.KEY_Return || symbol === Clutter.KEY_KP_Enter ||
                   symbol === Clutter.KEY_space || symbol === Clutter.KEY_Up ||
                   symbol === Clutter.KEY_Down || symbol === Clutter.KEY_Left ||
                   symbol === Clutter.KEY_Right || symbol === Clutter.KEY_Delete) {
            /* Swallowed on purpose, and named rather than left to fall
             * through `keysym_to_unicode`: these are the keys that would
             * ACTIVATE or EDIT something underneath, and v6 grabbed them for
             * exactly that reason (§5.1e). Key focus already keeps them from
             * the application; listing them keeps the intent readable. */
        } else {
            const ch = String.fromCharCode(Clutter.keysym_to_unicode(symbol));
            if (ch && ch !== '\0')
                this._typeChar(ch);
        }
        return Clutter.EVENT_STOP;
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

    /** Every window on the active workspace, **bottom to top in stacking
     *  order** — the one thing §4.3 said only the compositor could supply
     *  and then went eleven months without asking for.
     *
     *  §4.3 scopes detection to the focused window, and the reason is
     *  correctness, not speed: *"AT-SPI has no concept of occlusion.
     *  Measured: Nautilus's `Search Everywhere` and `Main Menu` toggles both
     *  pass VISIBLE, SHOWING, SENSITIVE and have sane bounds while the
     *  window is completely buried under a maximized terminal."* Labelling a
     *  buried node produces a badge that clicks whatever is on top, which is
     *  the wrong-click outcome §4.5 exists to prevent.
     *
     *  The same paragraph names the missing piece — *"only the compositor's
     *  stacking order can [tell you a window is covered], which is one more
     *  thing the extension supplies"* — and this is it. muvor subtracts the
     *  windows above each window from that window's rectangle and hints
     *  what is left, so a target is labelled when it can be *seen*, which is
     *  the rule a user states as "if I can see it, it must be detected".
     *
     *  **The shell decides nothing here** (§5.5, D5). It reports geometry
     *  and two state bits and does not filter: whether a minimized window or
     *  a 1x1 utility window is worth hinting is muvor's question, and a
     *  shell that answered it would be holding product logic.
     *
     *  Both rectangles for the same reason `FocusedWindow` reports both
     *  (§5.1h): the frame rect excludes the client-side shadow and the
     *  accessibility frame is the buffer, so occlusion is computed against
     *  the frame — a shadow does not hide anything — while the a11y origin
     *  correction wants the buffer.
     */
    Windows() {
        const wsm = global.display.get_workspace_manager();
        const active = wsm.get_active_workspace();
        /* `sort_windows_by_stacking` returns bottom-to-top, which is the
         * order muvor wants: it walks upwards accumulating what is covered. */
        const stacked = global.display.sort_windows_by_stacking(
            active.list_windows());
        const out = [];
        for (const win of stacked) {
            const rect = win.get_frame_rect();
            const buf = win.get_buffer_rect();
            out.push([
                win.get_title() ?? '',
                rect.x, rect.y, rect.width, rect.height,
                win.get_pid(),
                win.get_wm_class() ?? '',
                buf.x, buf.y, buf.width, buf.height,
                /* Minimized, and "would be showing if nothing covered it" —
                 * the second catches a window hidden for reasons that are
                 * not minimization, which from muvor's side look the same. */
                win.minimized,
                win.showing_on_its_workspace(),
            ]);
        }
        return out;
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
     *  was tapped or is being held (§12.3), and since v8 that is a question
     *  with an answer rather than a guess.
     *
     *  Three ways this ends and all of them end it:
     *    - the ' prefix was armed  -> movement mode, no waiting at all
     *    - the key comes up        -> a tap. The user's own finger, 40-120 ms
     *    - HOLD_MS with the key still down -> a hold
     *
     *  v7 had a fourth: "nobody is watching this key, so call it a tap".
     *  That was the only one that ever fired, because the key it needed to
     *  watch was consumed by mutter before any stage signal existed (§12.13).
     *  Holding the key focus removes the case rather than tuning it. */
    _complete(label) {
        if (this._armed) {
            this._armed = false;
            this._enterKeyGrab();
            this._emitTypedHold(label);
            return;
        }
        this._pending = {label, timer: null};
        this._pending.timer = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT, HOLD_MS, () => {
                if (this._pending)
                    this._pending.timer = null;
                this._resolve(true);
                return GLib.SOURCE_REMOVE;
            });
    }

    /** Settle a pending label. `held` decides which signal muvor gets.
     *
     *  A hold hands the keyboard straight over rather than dropping it and
     *  waiting to be asked: the key is still down, and a gap where nobody
     *  holds it is a release leaking into the application. Since v8 that is
     *  one line — the same actor keeps the same key focus and only changes
     *  what it does with what arrives — so there is no gap left to leak
     *  through at all. */
    _resolve(held) {
        const pending = this._pending;
        if (!pending)
            return;
        this._pending = null;
        if (pending.timer)
            GLib.source_remove(pending.timer);
        if (held) {
            this._enterKeyGrab();
            this._emitTypedHold(pending.label);
        } else {
            this._teardown();
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
        this._setState('idle');
        if (this._refocusId) {
            GLib.source_remove(this._refocusId);
            this._refocusId = 0;
        }
        /* **The key focus goes back before anything is destroyed.** An
         * overlay torn down while it still holds it leaves a keyboard that
         * types into a dead actor, and that is the one failure in this file
         * a user cannot recover from without reaching for the mouse. First,
         * unconditionally, and again in `disable`.
         *
         * The other two input paths are released here too, and releasing one
         * that is not held is a no-op — a v8 session never holds either. */
        this._releaseKeyFocus();
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
            if (this._btnId) {
                this._overlay.disconnect(this._btnId);
                this._btnId = null;
            }
            if (this._focusOutId) {
                this._overlay.disconnect(this._focusOutId);
                this._focusOutId = null;
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
     *  **Since v8 this is a mode switch, not a second grab.** The overlay
     *  that spelled the label already holds the key focus, so the hold path
     *  changes one field and keeps the same actor, the same focus and the
     *  same handlers. That closes the gap v7's comment worried about by
     *  construction: there is no instant between the label completing and
     *  movement mode starting in which the still-down key belongs to the
     *  application.
     *
     *  Called cold by `GrabKeys` (free mode), where there is no overlay yet
     *  and one has to be made. */
    _enterKeyGrab() {
        this._setState('keys');
        /* The badges are done; the mode that replaces them wants the screen
         * visible. Drop every child — the badges AND §5.1c's cyan dots,
         * which are children of the overlay and not of `_actors`, and would
         * otherwise sit on screen for the whole of movement mode marking
         * where the pointer used to be about to go. Keep the widget, its key
         * focus and its handlers. */
        this._overlay?.destroy_all_children();
        this._actors = [];
        this._targets = [];
        this._typed = '';

        if (!this._overlay) {
            this._overlay = new St.Widget({
                style_class: 'muvor-overlay',
                reactive: true,
                can_focus: true,
                x: 0,
                y: 0,
                width: global.stage.width,
                height: global.stage.height,
            });
            Main.layoutManager.addChrome(this._overlay);
            this._takeKeyFocus(this._overlay);
            this._connectKeys();
        } else if (!this._hasKeyFocus && !this._grab) {
            /* The accelerator fuse is in force: the hint had no key focus to
             * inherit, so movement mode takes one now. It still costs no
             * compositor grab. */
            this._releaseKeys();
            if (this._accelId) {
                global.display.disconnect(this._accelId);
                this._accelId = null;
            }
            this._takeKeyFocus(this._overlay);
            this._connectKeys();
        }

        /* `_setState` above ran before free mode's cold path had an actor to
         * conceal, and the accelerator fuse can have replaced the handlers
         * since. Applying it here is what makes "concealed" true of the
         * overlay that actually exists, on all three entries. */
        this._applyInputOwnership();

        /* Free mode is a *mode*: a user is in it until they leave, and 10 s
         * of thinking is not a wedge. But it is still a keyboard the user is
         * not typing into, so it still ends by itself — six times the hint
         * deadman, and the same argument (§5.1b). Since v10 every key in
         * `keys` state re-arms it, so the sixty seconds are sixty seconds of
         * *silence* and not sixty seconds of the mode. */
        this._armDeadman(DEADMAN_SECONDS * 6);
    }

}
