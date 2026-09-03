//! muvor's pure half: what a target is *called*, and what happens as it is
//! typed.
//!
//! Nothing here talks to a bus, a socket or a compositor. It is given
//! positions and returns labels, which is why its tests can prove their
//! claims outright rather than measure them (§8).
//!
//! `plan.md` §5.3 and D5 are normative.

pub mod detect;
pub mod free;
pub mod label;
pub mod motion;
pub mod occlude;
pub mod place;

pub use detect::{
    grid, opaque_block, revalidate, targets_in, Claim as CvClaim, Fingerprint, Found, Params,
    Rect, Verdict as CvVerdict, MAX_DRIFT, MIN_IOU,
};
pub use free::{Dir, Free};
pub use occlude::{is_covered, visible_fraction};
pub use motion::{Button, Cmd as MotionCmd, Ev as MotionEv, Key as MotionKey, Motion};
pub use label::{Alphabet, Labels, Point, Progress, Typing};
pub use place::{place, place_all, Placement, Side, BADGE_H, BADGE_W};
