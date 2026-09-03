//! Finding clickable things where the accessibility tree says nothing
//! (§5.4, §5.4e, D11, D16).
//!
//! **Scoped by measurement, not by ambition.** §5.4e's first corpus asked
//! what actually lives in the regions AT-SPI cannot explain, and the answer
//! came apart by shape: gnome-terminal's 96%-opaque block holds nothing
//! discrete, while Nautilus's file view is one 892×592 rectangle holding
//! **twenty file icons in a 5×4 grid**. D11 said an opaque region gets one
//! centre target rather than a detector, and it is right about the first
//! shape and wrong about the second.
//!
//! So this module does one job: **find a grid of discrete items inside a
//! region the tree could not explain.** That is a narrower problem than a
//! general region detector, and it is narrower for a reason that was
//! measured rather than argued.
//!
//! **D11 is the degenerate case, and it is spelled out rather than relied
//! on.** When there is no grid — a terminal, a video, a canvas — the
//! projections usually produce a single band in each axis and [`grid`]
//! returns one cell covering the region, which is D11's answer arriving on
//! its own. But "usually" is not "always": a region whose gradient is
//! perfectly uniform has nothing above the adaptive threshold and [`grid`]
//! honestly returns *nothing*. Leaving D11 to emerge from that would mean an
//! opaque region occasionally offering no target at all, which is the one
//! outcome D11 exists to prevent. So [`grid`] reports what it found and
//! [`targets_in`] applies the rule.
//!
//! Pure, like the rest of this crate: pixels in, rectangles out, no I/O and
//! no knowledge of where either came from.

/// A rectangle in whatever space the caller is working in. Screen
/// coordinates everywhere this crate is used, but nothing here depends on
/// that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn centre(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub const fn area(&self) -> i64 {
        self.w as i64 * self.h as i64
    }
}

/// The largest axis-aligned rectangle inside `area` that none of `occupied`
/// touches — "the rectangle the tree could not explain", which is D10's
/// original phrase for the thing a capture should be scoped to.
///
/// Returns `None` when `occupied` covers everything, or when `area` is
/// degenerate.
///
/// The classical maximal-rectangle-in-a-binary-matrix scan: one row
/// histogram of free height, and a monotonic stack per row. Linear in the
/// pixels of `area`, which is why the whole thing is affordable at 1080p
/// despite looking quadratic.
pub fn opaque_block(area: Rect, occupied: &[Rect]) -> Option<Rect> {
    if area.w <= 0 || area.h <= 0 {
        return None;
    }
    let (w, h) = (area.w as usize, area.h as usize);
    let mut taken = vec![false; w * h];
    for r in occupied {
        // Clipped into `area`'s own coordinates, and saturating rather than
        // wrapping: §4.4 lets a degenerate rectangle carry i32::MIN, and a
        // rectangle that arrives as nonsense must not be able to index.
        let x0 = (r.x - area.x).clamp(0, area.w) as usize;
        let y0 = (r.y - area.y).clamp(0, area.h) as usize;
        let x1 = (r.x.saturating_add(r.w) - area.x).clamp(0, area.w) as usize;
        let y1 = (r.y.saturating_add(r.h) - area.y).clamp(0, area.h) as usize;
        for row in y0..y1 {
            taken[row * w + x0..row * w + x1].fill(true);
        }
    }

    let mut best: Option<Rect> = None;
    let mut heights = vec![0u32; w];
    // (left edge of the run this bar could extend back to, bar height)
    let mut stack: Vec<(usize, u32)> = Vec::with_capacity(w + 1);
    for y in 0..h {
        for x in 0..w {
            heights[x] = if taken[y * w + x] { 0 } else { heights[x] + 1 };
        }
        stack.clear();
        // One past the end, with a zero sentinel, so the last run is closed
        // by the same code that closes every other one.
        let sentinel = heights.iter().copied().chain(std::iter::once(0));
        for (x, cur) in sentinel.enumerate() {
            let mut start = x;
            while let Some(&(s, ht)) = stack.last() {
                if ht < cur {
                    break;
                }
                stack.pop();
                let candidate = Rect::new(
                    area.x + s as i32,
                    area.y + (y + 1 - ht as usize) as i32,
                    (x - s) as i32,
                    ht as i32,
                );
                if candidate.area() > best.map_or(0, |b| b.area()) {
                    best = Some(candidate);
                }
                start = s;
            }
            stack.push((start, cur));
        }
    }
    best.filter(|b| b.area() > 0)
}

/// Runs of "there is something here" in a one-dimensional projection.
///
/// A run ends only at a gap of at least `min_gap`, so the space *inside* a
/// glyph does not split a word and the space inside an icon does not split
/// the icon. Runs shorter than `min_run` are dropped as noise.
fn bands(profile: &[u32], min_gap: usize, min_run: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = profile.len();
    let mut i = 0;
    while i < n {
        if profile[i] == 0 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n {
            if profile[j] > 0 {
                j += 1;
                continue;
            }
            let mut k = j;
            while k < n && profile[k] == 0 {
                k += 1;
            }
            if k - j >= min_gap {
                break;
            }
            j = k;
        }
        if j - i >= min_run {
            out.push((i, j));
        }
        i = j;
    }
    out
}

/// Merge bands separated by a gap belonging to the *small* population.
///
/// An icon and the filename under it are two bands with a small gap between
/// them; two rows of files are separated by a large one. Both populations
/// exist in the same projection and the split between them is a property of
/// the theme, the icon size and the font — so it is **found, not written
/// down**: sort the gaps, take the largest relative jump, and cut there.
/// Change the icon size and the threshold moves on its own.
///
/// A jump smaller than `SPLIT` means one population and nothing to merge,
/// which is the ordinary case for a region that is not a grid at all.
fn merge_bands(bs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    /// How much bigger the between-item gap must be than the within-item one
    /// before the two are believed to be different populations. 1.4 is the
    /// loosest value that still refuses a uniform run; Nautilus's measured
    /// split is 1.57 (13–14 px within an entry, 22–25 px between rows).
    const SPLIT: f32 = 1.4;

    if bs.len() < 3 {
        return bs.to_vec();
    }
    let mut gaps: Vec<f32> =
        bs.windows(2).map(|p| (p[1].0.saturating_sub(p[0].1)) as f32).collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut best = (0.0f32, 0usize);
    for k in 0..gaps.len() - 1 {
        let ratio = gaps[k + 1] / gaps[k].max(1.0);
        if ratio > best.0 {
            best = (ratio, k);
        }
    }
    if best.0 < SPLIT {
        return bs.to_vec();
    }
    let threshold = (gaps[best.1] + gaps[best.1 + 1]) / 2.0;

    let mut out = vec![bs[0]];
    for &(a, b) in &bs[1..] {
        let last = out.last_mut().unwrap_or_else(|| unreachable!("seeded above"));
        if ((a.saturating_sub(last.1)) as f32) < threshold {
            last.1 = b;
        } else {
            out.push((a, b));
        }
    }
    out
}

/// What [`grid`] needs to know that is not the pixels.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Smallest gap in a projection that separates two bands, in pixels.
    pub min_gap: usize,
    /// Shortest band worth keeping, in pixels.
    pub min_run: usize,
    /// Fraction of a cell that must carry an edge before the cell is
    /// believed to hold something. Rejects the empty intersections of a
    /// ragged last row.
    pub min_fill: f32,
}

impl Default for Params {
    /// Measured against §5.4e's corpus at 1920×1080, scale 1. Every one of
    /// these is a pixel count and every pixel count is a scale assumption:
    /// see [`Params::for_scale`].
    fn default() -> Self {
        Self { min_gap: 12, min_run: 8, min_fill: 0.02 }
    }
}

impl Params {
    /// The same parameters at a different display scale.
    ///
    /// The three defaults were tuned on a scale-1 frame and two of them are
    /// distances, so a HiDPI frame needs them multiplied or the gaps it
    /// finds are half the size the thresholds expect. `min_fill` is a ratio
    /// and does not move.
    pub fn for_scale(scale: f64) -> Self {
        let d = Self::default();
        let k = scale.max(1.0);
        Self {
            min_gap: (d.min_gap as f64 * k).round() as usize,
            min_run: (d.min_run as f64 * k).round() as usize,
            min_fill: d.min_fill,
        }
    }
}

/// Discrete items inside `block`, found from edges alone.
///
/// `luma` is single-channel, `stride` bytes per row, and `block` must lie
/// inside it — in the image's own coordinate space, with `origin` naming
/// where that space starts so the returned rectangles come back in the
/// caller's. Returns one rectangle per item, tight around the item's own
/// content rather than around the cell it sits in.
///
/// Returns **empty** when the region carries no structure this can see —
/// a flat fill, or a texture so uniform that nothing rises above the
/// adaptive threshold. That is a real answer and not a failure; [`targets_in`]
/// is what turns it into D11's single centre target.
pub fn grid(luma: &[u8], stride: usize, block: Rect, origin: (i32, i32), p: Params) -> Vec<Rect> {
    let (bw, bh) = (block.w.max(0) as usize, block.h.max(0) as usize);
    if bw < 3 || bh < 3 {
        return Vec::new();
    }
    let (ox, oy) = ((block.x - origin.0).max(0) as usize, (block.y - origin.1).max(0) as usize);

    // Sobel magnitude, then a threshold that adapts to the region rather
    // than to a constant: a dark terminal and a light file manager have
    // nothing in common in absolute gradient, and every pixel constant here
    // is a theme this would stop working on.
    let at = |x: usize, y: usize| -> i32 { luma[(oy + y) * stride + ox + x] as i32 };
    let mut mag = vec![0f32; bw * bh];
    let mut sum = 0f64;
    for y in 1..bh - 1 {
        for x in 1..bw - 1 {
            let gx = -at(x - 1, y - 1) - 2 * at(x - 1, y) - at(x - 1, y + 1)
                + at(x + 1, y - 1)
                + 2 * at(x + 1, y)
                + at(x + 1, y + 1);
            let gy = -at(x - 1, y - 1) - 2 * at(x, y - 1) - at(x + 1, y - 1)
                + at(x - 1, y + 1)
                + 2 * at(x, y + 1)
                + at(x + 1, y + 1);
            let m = ((gx * gx + gy * gy) as f32).sqrt();
            mag[y * bw + x] = m;
            sum += m as f64;
        }
    }
    let n = (bw * bh) as f64;
    let mean = sum / n;
    let var = mag.iter().map(|&m| (m as f64 - mean).powi(2)).sum::<f64>() / n;
    let threshold = (mean + var.sqrt()) as f32;

    let edge: Vec<bool> = mag.iter().map(|&m| m > threshold).collect();
    let mut rows = vec![0u32; bh];
    let mut cols = vec![0u32; bw];
    for y in 0..bh {
        for x in 0..bw {
            if edge[y * bw + x] {
                rows[y] += 1;
                cols[x] += 1;
            }
        }
    }

    let row_bands = merge_bands(&bands(&rows, p.min_gap, p.min_run));
    let col_bands = merge_bands(&bands(&cols, p.min_gap, p.min_run));

    let mut out = Vec::new();
    for &(r0, r1) in &row_bands {
        for &(c0, c1) in &col_bands {
            // The cell's own content, not the cell: the bands are cut at
            // the widest extent of the whole row and column, so a short
            // filename in a wide column would otherwise get a rectangle
            // reaching into the whitespace beside it, and its centre would
            // miss.
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
            let mut lit = 0usize;
            for y in r0..r1 {
                for x in c0..c1 {
                    if edge[y * bw + x] {
                        lit += 1;
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                    }
                }
            }
            let cells = ((r1 - r0) * (c1 - c0)) as f32;
            if lit == 0 || (lit as f32 / cells) < p.min_fill {
                continue;
            }
            out.push(Rect::new(
                block.x + x0 as i32,
                block.y + y0 as i32,
                (x1 - x0 + 1) as i32,
                (y1 - y0 + 1) as i32,
            ));
        }
    }
    out
}

/// What [`targets_in`] concluded about a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// Discrete items, and how many. Nautilus's file view answers `Grid(20)`.
    Grid(usize),
    /// Nothing discrete: D11's rule applied, and the single rectangle
    /// returned is the region itself. A terminal, a video, a canvas.
    Single,
}

/// What to offer in a region the accessibility tree could not explain.
///
/// [`grid`] with **D11 applied**: a region that yields no discrete structure
/// still gets exactly one target, its centre, because "nothing happens" is
/// the failure mode D12 and D11 both exist to prevent. A region that yields
/// one item is the same answer arriving by the other road, and is reported
/// as [`Found::Single`] too — one rectangle covering the region is a centre
/// target whatever produced it.
///
/// Never returns an empty vector for a non-degenerate `block`.
pub fn targets_in(
    luma: &[u8],
    stride: usize,
    block: Rect,
    origin: (i32, i32),
    p: Params,
) -> (Vec<Rect>, Found) {
    let items = grid(luma, stride, block, origin, p);
    match items.len() {
        0 => (vec![block], Found::Single),
        // One item found is one target, but the *item's* rectangle is a
        // better target than the whole block: it is tight around whatever
        // was actually there, so its centre is on the thing rather than on
        // the margin beside it.
        1 => (items, Found::Single),
        n => (items, Found::Grid(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_block_is_the_whole_area_when_nothing_is_taken() {
        let a = Rect::new(10, 20, 100, 50);
        assert_eq!(opaque_block(a, &[]), Some(a));
    }

    #[test]
    fn opaque_block_is_none_when_everything_is_taken() {
        let a = Rect::new(0, 0, 40, 40);
        assert_eq!(opaque_block(a, &[Rect::new(0, 0, 40, 40)]), None);
    }

    #[test]
    fn opaque_block_finds_the_larger_of_two_free_regions() {
        // A 100x100 area with a full-width bar across y=30..40. The region
        // below it (60 tall) beats the one above (30 tall).
        let a = Rect::new(0, 0, 100, 100);
        let got = opaque_block(a, &[Rect::new(0, 30, 100, 10)]).expect("free space exists");
        assert_eq!(got, Rect::new(0, 40, 100, 60));
    }

    #[test]
    fn opaque_block_reports_in_the_areas_own_coordinates() {
        // The offset must survive: this is the bug that would put every
        // detected target one window-origin away from its pixels (D13).
        let a = Rect::new(458, 293, 100, 100);
        let got = opaque_block(a, &[Rect::new(458, 293, 100, 40)]).expect("free space exists");
        assert_eq!(got, Rect::new(458, 333, 100, 60));
    }

    #[test]
    fn opaque_block_survives_a_degenerate_rectangle() {
        // §4.4 lets a rejected node carry i32::MIN. It must clip, not panic.
        let a = Rect::new(0, 0, 20, 20);
        let got = opaque_block(a, &[Rect::new(i32::MIN, i32::MIN, 4, 4)]);
        assert_eq!(got, Some(a));
    }

    #[test]
    fn bands_split_only_on_a_wide_enough_gap() {
        //            0  1  2  3  4  5  6  7  8  9
        let p = [1u32, 1, 0, 1, 1, 0, 0, 0, 1, 1];
        // A one-wide gap at index 2 is inside a run; the three-wide gap ends it.
        assert_eq!(bands(&p, 3, 1), vec![(0, 5), (8, 10)]);
    }

    #[test]
    fn bands_drop_runs_shorter_than_min_run() {
        let p = [1u32, 0, 0, 0, 1, 1, 1, 1];
        assert_eq!(bands(&p, 3, 3), vec![(4, 8)]);
    }

    #[test]
    fn merge_bands_pairs_an_icon_with_its_label() {
        // Nautilus's measured geometry: 86px icon, 13px gap, 17px label,
        // then 22px to the next row.
        let bs = [(41, 127), (140, 157), (179, 265), (278, 295)];
        assert_eq!(merge_bands(&bs), vec![(41, 157), (179, 295)]);
    }

    #[test]
    fn merge_bands_leaves_a_uniform_run_alone() {
        // Evenly spaced bands are one population: nothing to merge, and
        // merging them would fuse a whole row into one target.
        let bs = [(0, 10), (20, 30), (40, 50), (60, 70)];
        assert_eq!(merge_bands(&bs), bs.to_vec());
    }

    /// Paint `n` bright squares on a dark field and check they come back.
    fn synthetic_grid(cols: usize, rows: usize) -> (Vec<u8>, usize, usize) {
        let (pitch, size, margin) = (60usize, 30usize, 15usize);
        let (w, h) = (margin * 2 + pitch * cols, margin * 2 + pitch * rows);
        let mut buf = vec![0u8; w * h];
        for r in 0..rows {
            for c in 0..cols {
                let (x0, y0) = (margin + c * pitch, margin + r * pitch);
                for y in y0..y0 + size {
                    buf[y * w + x0..y * w + x0 + size].fill(255);
                }
            }
        }
        (buf, w, h)
    }

    #[test]
    fn grid_finds_every_item_of_a_synthetic_grid() {
        let (buf, w, h) = synthetic_grid(5, 4);
        let got = grid(&buf, w, Rect::new(0, 0, w as i32, h as i32), (0, 0), Params::default());
        assert_eq!(got.len(), 20, "5x4 squares should give 20 cells, got {got:?}");
        // Every cell's centre must land inside a painted square.
        for r in &got {
            let (cx, cy) = r.centre();
            assert_eq!(
                buf[cy as usize * w + cx as usize], 255,
                "centre {cx},{cy} landed off the square"
            );
        }
        assert!(h > 0);
    }

    /// Deterministic noise — the shape of a video frame or a photograph, and
    /// D11's case.
    ///
    /// It has to be *irregular*. Any perfectly regular synthetic pattern
    /// gives a two-valued gradient field, and a two-valued field can fail to
    /// clear its own mean + sigma however strong its edges are (see
    /// `grid_finds_nothing_in_a_perfectly_uniform_field`). Real content has
    /// a spread; a checkerboard does not. This is the trap that makes
    /// synthetic negatives worth less than they look.
    fn dense_texture(w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![0u8; w * h];
        let mut state: u64 = 12345;
        for p in buf.iter_mut() {
            state = state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7fff_ffff;
            *p = ((state >> 16) & 0xff) as u8;
        }
        buf
    }

    #[test]
    fn grid_returns_one_cell_when_there_is_no_grid() {
        let (w, h) = (200usize, 120usize);
        let buf = dense_texture(w, h);
        let got = grid(&buf, w, Rect::new(0, 0, w as i32, h as i32), (0, 0), Params::default());
        assert_eq!(got.len(), 1, "a region with no gaps is one target (D11), got {got:?}");
    }

    #[test]
    fn grid_finds_nothing_in_a_perfectly_uniform_field() {
        // A checkerboard's gradient magnitude is the same at every pixel, so
        // nothing clears mean + sigma. `grid` says so honestly rather than
        // inventing a target — which is exactly why D11 is applied by
        // `targets_in` and not left to emerge from this.
        let (w, h) = (120usize, 80usize);
        let mut buf = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                buf[y * w + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let got = grid(&buf, w, Rect::new(0, 0, w as i32, h as i32), (0, 0), Params::default());
        assert!(got.is_empty(), "expected an honest nothing, got {got:?}");
    }

    #[test]
    fn targets_in_always_offers_something_d11_can_click() {
        let (w, h) = (120usize, 80usize);
        let block = Rect::new(0, 0, w as i32, h as i32);
        // The uniform field `grid` finds nothing in.
        let mut buf = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                buf[y * w + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let (got, found) = targets_in(&buf, w, block, (0, 0), Params::default());
        assert_eq!(found, Found::Single);
        assert_eq!(got, vec![block], "D11: the region itself is the target");

        // And a flat fill, which is the other way to find nothing.
        let flat = vec![17u8; w * h];
        let (got, found) = targets_in(&flat, w, block, (0, 0), Params::default());
        assert_eq!(found, Found::Single);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn targets_in_reports_a_real_grid_as_a_grid() {
        let (buf, w, h) = synthetic_grid(5, 4);
        let (got, found) =
            targets_in(&buf, w, Rect::new(0, 0, w as i32, h as i32), (0, 0), Params::default());
        assert_eq!(found, Found::Grid(20));
        assert_eq!(got.len(), 20);
        assert!(h > 0);
    }

    #[test]
    fn grid_returns_nothing_for_a_blank_region() {
        let buf = vec![17u8; 100 * 100];
        let got = grid(&buf, 100, Rect::new(0, 0, 100, 100), (0, 0), Params::default());
        assert!(got.is_empty(), "a flat region has nothing to click, got {got:?}");
    }

    #[test]
    fn grid_reports_in_the_callers_coordinates() {
        let (buf, w, h) = synthetic_grid(2, 2);
        let origin = (458, 293);
        let block = Rect::new(origin.0, origin.1, w as i32, h as i32);
        let got = grid(&buf, w, block, origin, Params::default());
        assert_eq!(got.len(), 4);
        for r in &got {
            assert!(r.x >= origin.0 && r.y >= origin.1, "{r:?} is not in screen space");
        }
        assert!(h > 0);
    }

    #[test]
    fn params_scale_the_distances_and_not_the_ratio() {
        let d = Params::default();
        let two = Params::for_scale(2.0);
        assert_eq!(two.min_gap, d.min_gap * 2);
        assert_eq!(two.min_run, d.min_run * 2);
        assert!((two.min_fill - d.min_fill).abs() < f32::EPSILON);
    }
}

/// A CV target's identity: what its rectangle looked like when it was found
/// (§4.5b).
///
/// A 64×64 **nearest-neighbour** luma sample. Nearest rather than averaged,
/// and 64 rather than the 8 an average-hash would use, for a measured
/// reason: downsampling destroys the filename text under an icon, and on a
/// file manager the filename is the only thing that distinguishes two plain
/// folders. Measured on §5.4d's corpus of twenty Nautilus items, the closest
/// confusing pair (`venvs` ~ `.cache`) scores 0.36 at 8×8 averaged and
/// **2.85** at 64×64 nearest, against **0.000** for the same item across two
/// captures of a static window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    cells: [u8; Self::N * Self::N],
}

impl Fingerprint {
    const N: usize = 64;

    /// Sample `rect` out of a luma image. `origin` names where the image's
    /// `0,0` sits, so `rect` can be in the caller's coordinates.
    ///
    /// A rectangle partly outside the image samples the edge rather than
    /// failing: a window hanging off the screen is an ordinary thing and a
    /// fingerprint of the visible part is still worth comparing.
    pub fn of(luma: &[u8], stride: usize, rect: Rect, origin: (i32, i32)) -> Self {
        let mut cells = [0u8; Self::N * Self::N];
        let (w, h) = (rect.w.max(1) as usize, rect.h.max(1) as usize);
        let (ox, oy) = (rect.x - origin.0, rect.y - origin.1);
        let rows = luma.len().checked_div(stride).unwrap_or(0);
        for cy in 0..Self::N {
            for cx in 0..Self::N {
                // Nearest neighbour, centre-sampled.
                let sx = ox + ((cx * 2 + 1) * w / (Self::N * 2)) as i32;
                let sy = oy + ((cy * 2 + 1) * h / (Self::N * 2)) as i32;
                let sx = sx.clamp(0, stride.saturating_sub(1) as i32) as usize;
                let sy = sy.clamp(0, rows.saturating_sub(1) as i32) as usize;
                cells[cy * Self::N + cx] = luma.get(sy * stride + sx).copied().unwrap_or(0);
            }
        }
        Self { cells }
    }

    /// Mean absolute difference. Zero for identical pixels.
    pub fn distance(&self, other: &Self) -> f32 {
        let total: u32 = self
            .cells
            .iter()
            .zip(&other.cells)
            .map(|(a, b)| a.abs_diff(*b) as u32)
            .sum();
        total as f32 / (Self::N * Self::N) as f32
    }
}

/// A CV target, as claimed by whatever drew a badge on it.
#[derive(Debug, Clone)]
pub struct Claim {
    /// Where the item was when it was found.
    pub rect: Rect,
    /// Where the click will go.
    pub click: (i32, i32),
    /// What it looked like then.
    pub print: Fingerprint,
}

/// Why a CV click was refused, or that it was not (§4.5b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both checks passed: an item is still there and still looks the same.
    Ok,
    /// Nothing was detected under the click point any more.
    Gone,
    /// Something is there, but it is not where the claim said — the view
    /// scrolled, or the window moved.
    Moved,
    /// The rectangle matches and the pixels do not. An in-place content
    /// change: the same slot, holding something else.
    Changed,
}

impl Verdict {
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }

    pub const fn why(self) -> &'static str {
        match self {
            Self::Ok => "still there, and still looks the same",
            Self::Gone => "nothing is detected under the click point any more",
            Self::Moved => "an item is there but not where it was — the view moved",
            Self::Changed => "the rectangle is the same and the pixels are not — \
                              the slot now holds something else",
        }
    }
}

/// How much a fresh rectangle may differ from the claimed one, as
/// intersection over union. 0.5 is the conventional detection threshold and
/// it is far tighter than it needs to be for the case that matters: a grid
/// scrolled by one row scores 0.
pub const MIN_IOU: f32 = 0.5;

/// How far the pixels may drift before the item is believed to have been
/// replaced. **Provisional** — it sits well below the 2.85 measured
/// confusion floor and well above the 0.000 of a static re-capture, but the
/// distance a *legitimately redrawn* item travels has not been measured.
/// See §4.5b.
pub const MAX_DRIFT: f32 = 1.0;

fn iou(a: Rect, b: Rect) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.x.saturating_add(a.w).min(b.x.saturating_add(b.w));
    let y1 = a.y.saturating_add(a.h).min(b.y.saturating_add(b.h));
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let inter = (x1 - x0) as i64 * (y1 - y0) as i64;
    let union = a.area() + b.area() - inter;
    if union <= 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

const fn contains(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.0 < r.x.saturating_add(r.w) && p.1 >= r.y && p.1 < r.y.saturating_add(r.h)
}

/// §4.5b: is this CV target still the thing the label promised?
///
/// `fresh` is a re-detection over a freshly captured `luma`. Both checks
/// from §4.5b, in order — the structural one first because it is the one
/// that localises the failure, and the identity one second because it only
/// means anything once the rectangle has been agreed on.
pub fn revalidate(
    claim: &Claim,
    fresh: &[Rect],
    luma: &[u8],
    stride: usize,
    origin: (i32, i32),
) -> Verdict {
    let Some(&found) = fresh.iter().find(|r| contains(**r, claim.click)) else {
        return Verdict::Gone;
    };
    if iou(found, claim.rect) < MIN_IOU {
        return Verdict::Moved;
    }
    if Fingerprint::of(luma, stride, found, origin).distance(&claim.print) > MAX_DRIFT {
        return Verdict::Changed;
    }
    Verdict::Ok
}

#[cfg(test)]
mod revalidation {
    use super::*;

    fn flat(v: u8, w: usize, h: usize) -> Vec<u8> {
        vec![v; w * h]
    }

    /// A field with a distinguishable patch at `rect`, so a fingerprint of
    /// it is not the same as a fingerprint of anywhere else.
    fn patched(w: usize, h: usize, rect: Rect, v: u8) -> Vec<u8> {
        let mut buf = flat(10, w, h);
        for y in rect.y.max(0) as usize..(rect.y + rect.h).min(h as i32) as usize {
            for x in rect.x.max(0) as usize..(rect.x + rect.w).min(w as i32) as usize {
                buf[y * w + x] = v;
            }
        }
        buf
    }

    fn claim_at(rect: Rect, luma: &[u8], w: usize) -> Claim {
        Claim {
            rect,
            click: rect.centre(),
            print: Fingerprint::of(luma, w, rect, (0, 0)),
        }
    }

    #[test]
    fn unchanged_is_ok() {
        let (w, h) = (200usize, 200usize);
        let r = Rect::new(20, 20, 60, 60);
        let buf = patched(w, h, r, 240);
        let c = claim_at(r, &buf, w);
        assert_eq!(revalidate(&c, &[r], &buf, w, (0, 0)), Verdict::Ok);
    }

    #[test]
    fn nothing_under_the_point_is_gone() {
        let (w, h) = (200usize, 200usize);
        let r = Rect::new(20, 20, 60, 60);
        let buf = patched(w, h, r, 240);
        let c = claim_at(r, &buf, w);
        // A re-detection that found something, but not here.
        let elsewhere = Rect::new(120, 120, 60, 60);
        assert_eq!(revalidate(&c, &[elsewhere], &buf, w, (0, 0)), Verdict::Gone);
        assert_eq!(revalidate(&c, &[], &buf, w, (0, 0)), Verdict::Gone);
    }

    #[test]
    fn a_partly_scrolled_item_is_refused_as_moved() {
        // The view scrolled by less than a full row, so the click point is
        // still inside the item but the item is no longer where the badge
        // said. IoU here is 35/85 = 0.41, under MIN_IOU.
        let (w, h) = (200usize, 400usize);
        let r = Rect::new(20, 20, 60, 60);
        let buf = patched(w, h, r, 240);
        let c = claim_at(r, &buf, w);
        let scrolled = Rect::new(20, 45, 60, 60);
        assert!(contains(scrolled, c.click), "the test needs the point still inside");
        assert!(iou(scrolled, r) < MIN_IOU);
        assert_eq!(revalidate(&c, &[scrolled], &buf, w, (0, 0)), Verdict::Moved);
    }

    #[test]
    fn a_scroll_by_exactly_one_pitch_is_caught_by_the_fingerprint_alone() {
        // Found by writing the test above and getting the geometry wrong.
        // A grid that scrolls by exactly one row does NOT move any
        // rectangle: a different item slides into the same slot, so the
        // fresh detection reports the same rectangle, IoU is 1.0 and the
        // structural check passes clean. Only the pixels differ. This is
        // the strongest argument for §4.5b having two checks, and it is a
        // better one than the re-sort it was written for.
        let (w, h) = (200usize, 400usize);
        let slot = Rect::new(20, 20, 60, 60);
        let before = patched(w, h, slot, 240);
        let after = patched(w, h, slot, 40); // a different item, same slot
        let c = claim_at(slot, &before, w);
        assert_eq!(iou(slot, slot), 1.0, "the geometry is unchanged");
        assert_eq!(revalidate(&c, &[slot], &after, w, (0, 0)), Verdict::Changed);
    }

    #[test]
    fn a_few_pixels_of_movement_is_allowed() {
        // A window that shifted 2 px must not cost the user their click.
        let (w, h) = (200usize, 200usize);
        let r = Rect::new(20, 20, 60, 60);
        let moved = Rect::new(22, 22, 60, 60);
        let buf = patched(w, h, moved, 240);
        let c = Claim { rect: r, click: r.centre(), print: Fingerprint::of(&buf, w, moved, (0, 0)) };
        assert!(iou(moved, r) > MIN_IOU, "2px should be well inside tolerance");
        assert_eq!(revalidate(&c, &[moved], &buf, w, (0, 0)), Verdict::Ok);
    }

    #[test]
    fn same_rectangle_different_content_is_refused() {
        // The in-place re-sort: IoU is 1.0 and the pixels are not the same.
        // This is the check that IoU alone cannot make.
        let (w, h) = (200usize, 200usize);
        let r = Rect::new(20, 20, 60, 60);
        let before = patched(w, h, r, 240);
        let after = patched(w, h, r, 60);
        let c = claim_at(r, &before, w);
        assert_eq!(iou(r, r), 1.0);
        assert_eq!(revalidate(&c, &[r], &after, w, (0, 0)), Verdict::Changed);
    }

    #[test]
    fn a_fingerprint_of_itself_is_zero() {
        let (w, h) = (128usize, 128usize);
        let buf = patched(w, h, Rect::new(10, 10, 40, 40), 200);
        let r = Rect::new(0, 0, 128, 128);
        let a = Fingerprint::of(&buf, w, r, (0, 0));
        assert_eq!(a.distance(&a), 0.0);
    }

    #[test]
    fn a_fingerprint_keeps_detail_a_thumbnail_would_lose() {
        // Two fields differing only in a thin stripe — the stand-in for the
        // filename text that separates two identical folder icons. An 8x8
        // average would smear this to nearly nothing; 64x64 nearest keeps it.
        let (w, h) = (64usize, 64usize);
        let mut a = flat(0, w, h);
        let mut b = flat(0, w, h);
        for x in 0..w {
            a[30 * w + x] = 255;
            b[34 * w + x] = 255;
        }
        let r = Rect::new(0, 0, w as i32, h as i32);
        let d = Fingerprint::of(&a, w, r, (0, 0)).distance(&Fingerprint::of(&b, w, r, (0, 0)));
        assert!(d > MAX_DRIFT, "a moved stripe must read as different, got {d}");
    }

    #[test]
    fn fingerprint_survives_a_rectangle_off_the_edge() {
        let (w, h) = (32usize, 32usize);
        let buf = flat(99, w, h);
        // Half outside the image on two sides. Must not panic.
        let r = Rect::new(-8, 20, 40, 40);
        let f = Fingerprint::of(&buf, w, r, (0, 0));
        assert_eq!(f.distance(&f), 0.0);
    }

    #[test]
    fn iou_is_zero_for_disjoint_and_one_for_identical() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(iou(a, a), 1.0);
        assert_eq!(iou(a, Rect::new(50, 50, 10, 10)), 0.0);
    }
}
