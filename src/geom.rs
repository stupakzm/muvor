//! Pixels to device units, by measurement (plan.md §3.5).
//!
//! uictl's absolute pointer is ranged `0..ABS_RANGE_MAX` and knows nothing
//! about displays. libinput turns that into a `0..1` fraction and **the
//! compositor decides what rectangle to multiply it by** — a decision that is
//! mutter's private policy, is not queryable, differs per compositor, and may
//! have been fixed before any monitor was known.
//!
//! So muvor does not encode an answer. It injects two known device
//! coordinates, reads back where the pointer actually went, and solves the
//! line through the two points. Three injections, once, and the result is
//! correct whatever mutter is doing this week.
//!
//! The readback is the extension's `Pointer` — `global.get_pointer()` in
//! stage coordinates. It is the only channel: AT-SPI emits no mouse events
//! under mutter (measured 2026-08-18), which is why this file could be
//! written at M1 but not *verified* before M4.
//!
//! The maths is deliberately here, alone, and unit-tested without a
//! compositor: everything below the `calibrate` driver is arithmetic, and
//! arithmetic that only fails on someone else's desktop is not testable.

/// Where to sample. **Never 0 or `ABS_RANGE_MAX`** (§3.5): both clamp at the
/// screen edge, and a clamped sample fits a line through two points that are
/// not on it — silently, and with a plausible-looking result.
pub const LOW: i32 = 8192;
pub const HIGH: i32 = 24576;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// Both samples came back at the same place on this axis. The pointer
    /// did not move: injection failed, the readback is stale, or both
    /// samples clamped against the same edge.
    Degenerate { axis: char, at: i32 },
    /// The fit says the whole device range maps onto less than a pixel.
    Absurd { axis: char, span: f64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Degenerate { axis, at } => write!(
                f,
                "calibration: the pointer did not move on {axis} (both samples read {at}). \
                 Either uictl is not driving this seat, or the extension's readback is stale."
            ),
            Self::Absurd { axis, span } => write!(
                f,
                "calibration: the device range covers {span:.1} px on {axis} — not a display"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// One axis of the mapping: `stage = m * device + c`.
///
/// `f64` because `m` is a fraction of a pixel per device unit (~0.06 here)
/// and the inverse is what matters — integer arithmetic on the forward
/// direction would quantise the answer before it was used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axis {
    pub m: f64,
    pub c: f64,
}

impl Axis {
    /// The line through two (device, stage) samples.
    pub fn solve(axis: char, d0: i32, s0: i32, d1: i32, s1: i32) -> Result<Self, Error> {
        if s0 == s1 {
            return Err(Error::Degenerate { axis, at: s0 });
        }
        let m = f64::from(s1 - s0) / f64::from(d1 - d0);
        let c = f64::from(s0) - m * f64::from(d0);
        Ok(Self { m, c })
    }

    /// Stage pixels for a device unit — the direction that was measured.
    pub fn stage(&self, device: i32) -> f64 {
        self.m * f64::from(device) + self.c
    }

    /// Device units for a stage pixel — the direction that is used.
    pub fn device(&self, stage: f64) -> f64 {
        (stage - self.c) / self.m
    }
}

/// The measured mapping, both axes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    pub x: Axis,
    pub y: Axis,
    /// The `abs_range_max` the samples were taken against. Carried so the
    /// clamp cannot drift away from the device the fit describes.
    pub max: u32,
}

impl Mapping {
    /// Solve both axes from two samples.
    ///
    /// `lo`/`hi` are the device coordinates injected; `a`/`b` are where the
    /// compositor said the pointer landed.
    pub fn solve(lo: (i32, i32), a: (i32, i32), hi: (i32, i32), b: (i32, i32), max: u32) -> Result<Self, Error> {
        let x = Axis::solve('x', lo.0, a.0, hi.0, b.0)?;
        let y = Axis::solve('y', lo.1, a.1, hi.1, b.1)?;
        let m = Self { x, y, max };
        // A fit can be mathematically fine and physically nonsense — one
        // sample landing a pixel from the other produces a "display" tens of
        // pixels wide. Refuse it here rather than aim the pointer with it.
        let (w, h) = m.span();
        if w < 64.0 {
            return Err(Error::Absurd { axis: 'x', span: w });
        }
        if h < 64.0 {
            return Err(Error::Absurd { axis: 'y', span: h });
        }
        Ok(m)
    }

    /// The pixel rectangle the full device range covers.
    ///
    /// This is the finding §3.5 wants compared against the monitor layout: if
    /// it matches the union of all logical monitors, the device spans the
    /// desktop; if it matches one output, the device is bound to that output
    /// and the escape hatches apply.
    pub fn span(&self) -> (f64, f64) {
        let max = self.max as i32;
        (
            (self.x.stage(max) - self.x.stage(0)).abs(),
            (self.y.stage(max) - self.y.stage(0)).abs(),
        )
    }

    /// The origin of that rectangle — non-zero if the device is bound to a
    /// monitor that is not at `0,0`.
    pub fn origin(&self) -> (f64, f64) {
        (self.x.stage(0), self.y.stage(0))
    }

    /// A stage pixel as device units, clamped into range.
    ///
    /// Clamping rather than refusing: a coordinate slightly outside the
    /// mapped rectangle is a hint on a window that straddles an edge, and the
    /// nearest reachable pixel is the honest answer. A coordinate wildly
    /// outside is caught by validate-at-action (§4.5), not here.
    pub fn to_device(self, px: i32, py: i32) -> (i32, i32) {
        let max = f64::from(self.max);
        let clamp = |v: f64| v.round().clamp(0.0, max) as i32;
        (clamp(self.x.device(f64::from(px))), clamp(self.y.device(f64::from(py))))
    }
}

/// The fallback when there is no extension to read back from.
///
/// This is what `--screen` always was: an assumption that the device range
/// covers exactly one rectangle starting at the origin. It is kept because
/// `muvor poke 100 100` should work before anything else does, and it is
/// **not** what `muvor hint` uses.
pub fn assumed(width: i32, height: i32, max: u32) -> Mapping {
    let axis = |span: i32| Axis {
        m: f64::from(span - 1) / f64::from(max),
        c: 0.0,
    };
    Mapping { x: axis(width), y: axis(height), max }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic compositor: the one this machine is believed to have.
    /// 1920x1080 across 0..32767 is 17.07 device units per pixel (§3.5).
    fn stage_1920(d: i32) -> i32 {
        ((f64::from(d) / 32767.0) * 1919.0).round() as i32
    }

    #[test]
    fn solves_the_mapping_it_was_given() {
        let lo = (LOW, LOW);
        let hi = (HIGH, HIGH);
        let a = (stage_1920(LOW), stage_1920(LOW));
        let b = (stage_1920(HIGH), stage_1920(HIGH));
        let m = Mapping::solve(lo, a, hi, b, 32767).expect("fit");

        // Within 3 px of the real span, not exact, and the difference is
        // arithmetic rather than sloppiness: the readback is integer, so
        // each sample carries up to half a pixel of rounding, and the
        // samples sit at 25% and 75% of the range — so extrapolating to the
        // ends doubles that error. Two pixels across a whole screen, and
        // under a pixel anywhere between the samples, which is where every
        // real target is. Sampling wider would shrink it and would clamp
        // (§3.5), which is the worse trade.
        let (w, h) = m.span();
        assert!((w - 1919.0).abs() < 3.0, "span was {w}");
        assert!((h - 1919.0).abs() < 3.0);
        let (ox, oy) = m.origin();
        assert!(ox.abs() < 1.0 && oy.abs() < 1.0);
    }

    #[test]
    fn round_trips_within_a_pixel() {
        // §3.5 records the quantisation as a non-issue and this is the
        // check that keeps it recorded: 17 units per pixel, so no pixel can
        // be lost on the way back.
        let m = Mapping::solve(
            (LOW, LOW),
            (stage_1920(LOW), stage_1920(LOW)),
            (HIGH, HIGH),
            (stage_1920(HIGH), stage_1920(HIGH)),
            32767,
        )
        .expect("fit");
        for px in [0, 1, 640, 959, 960, 1279, 1919] {
            let (dx, _) = m.to_device(px, 0);
            let back = m.x.stage(dx).round() as i32;
            assert!((back - px).abs() <= 1, "{px} -> {dx} -> {back}");
        }
    }

    #[test]
    fn a_monitor_at_an_offset_is_just_a_different_c() {
        // The case that would break an assumed mapping and that a fit
        // handles without knowing it happened: the device bound to a second
        // output starting at x=1920.
        let s = |d: i32| 1920 + ((f64::from(d) / 32767.0) * 2559.0).round() as i32;
        let m = Mapping::solve((LOW, LOW), (s(LOW), 0), (HIGH, HIGH), (s(HIGH), 1079), 32767)
            .expect("fit");
        let (ox, _) = m.origin();
        assert!((ox - 1920.0).abs() < 2.0, "origin was {ox}");
        let (dx, _) = m.to_device(1920, 0);
        assert!(dx.abs() < 20, "left edge should be near 0, was {dx}");
        let (dx, _) = m.to_device(4479, 0);
        assert!((dx - 32767).abs() < 20, "right edge should be near max, was {dx}");
    }

    #[test]
    fn a_pointer_that_did_not_move_is_refused() {
        // The failure this whole file exists to make loud: uictl not driving
        // the seat, or a readback that never updates, would otherwise fit a
        // perfectly confident line through one point.
        let e = Mapping::solve((LOW, LOW), (500, 500), (HIGH, HIGH), (500, 500), 32767);
        assert_eq!(e, Err(Error::Degenerate { axis: 'x', at: 500 }));
    }

    #[test]
    fn a_clamped_axis_is_refused() {
        // Both samples against the same edge — what sampling at 0 and 32767
        // would produce, and the reason §3.5 forbids those points.
        let e = Mapping::solve((LOW, LOW), (0, 100), (HIGH, HIGH), (0, 900), 32767);
        assert_eq!(e, Err(Error::Degenerate { axis: 'x', at: 0 }));
    }

    #[test]
    fn a_fit_that_is_not_a_display_is_refused() {
        // Mathematically a fine line, physically a 40 px screen.
        let e = Mapping::solve((LOW, LOW), (100, 100), (HIGH, HIGH), (120, 900), 32767);
        assert!(matches!(e, Err(Error::Absurd { axis: 'x', .. })), "{e:?}");
    }

    #[test]
    fn the_assumed_mapping_is_the_old_arithmetic() {
        // `assumed` must agree with what `muvor poke` did before calibration
        // existed, or the fallback changes behaviour that was already
        // proven against a live daemon at M1.
        let m = assumed(1920, 1080, 32767);
        let old = |px: i64, span: i64| ((px * 32767) / (span - 1)).clamp(0, 32767) as i32;
        for px in [0, 1, 959, 960, 1919] {
            let (dx, _) = m.to_device(px as i32, 0);
            assert!((dx - old(px, 1920)).abs() <= 1, "{px}: {dx} vs {}", old(px, 1920));
        }
    }
}
