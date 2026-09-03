//! Stable spatial order, the alphabet, and the prefix trie (D5, §5.3).
//!
//! Three separable jobs, deliberately kept separate:
//!
//! 1. **Order** — put targets in reading order, deterministically.
//! 2. **Assign** — hand out labels in that order.
//! 3. **Type** — walk a trie as keys arrive, and say when a label is
//!    complete, still ambiguous, or wrong.
//!
//! **Why order, and not distance (D5).** A nearest-first assignment gives the
//! closest button the shortest label, which sounds ideal until an unrelated
//! element appears and every label moves. Muscle memory is the product; a
//! label that moves for reasons the user cannot see is worse than a label
//! that is one row further down. Reading order changes only when the tree
//! changes, and the same tree always produces the same labels — which is
//! M3's exit criterion, and a test below.

/// A target's centre — the only thing labelling needs to know about it.
///
/// Not a rectangle: labels are placed by the overlay, and ordering depends on
/// where a thing *is*, not how big it is. Keeping the input this narrow is
/// what stops this crate depending on detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Targets within this many pixels of each other vertically are one row.
///
/// A toolbar's buttons are rarely pixel-aligned — gnome-terminal's are at
/// y=0 and y=8 — and a strict `sort_by(y)` would read them as four separate
/// rows and label them top-to-bottom down a line that is visually
/// left-to-right. Rows are grown from their first member rather than
/// computed by dividing y, so two targets a pixel apart can never fall on
/// opposite sides of a boundary.
pub const ROW_BAND: i32 = 16;

/// The default alphabet: home row, left hand to right, no reaches.
///
/// Ten characters is exactly 100 two-character labels (§5.3), which the
/// measurements of §4.2-i say is ample — the busiest real window found was
/// 33 targets.
pub const HOME_ROW: &str = "asdfghjkl;";

/// The rank alphabet: what the *second* key counts with, inside a column
/// (§5.3c).
///
/// Home row first, because that is where the hand is. Then the top row, then
/// the bottom row, then the same three shifted — so the reach grows only as
/// a column gets crowded, and the first ten targets in any column are still
/// pure home row. Sixty ranks is a column of sixty targets on two keys; the
/// busiest real window measured (§4.2-i) had 33 in the whole window.
///
/// Order within each row is left to right, matching the columns' own
/// left-to-right order — one direction in the product, not one per axis.
pub const RANK_ROWS: &str = concat!(
    "asdfghjkl;",  // middle, unshifted — where the hand already is
    "qwertyuiop",  // top
    "zxcvbnm,./",  // bottom
    "ASDFGHJKL:",  // middle, shifted
    "QWERTYUIOP",  // top, shifted
    "ZXCVBNM<>?",  // bottom, shifted
);

/// Labels are always **at least** this long.
///
/// A window with four targets could be labelled with one key each, and it is
/// tempting. It is also how "Menu is k" silently becomes "Menu is ka" the
/// first time a fifth button appears — the label length changing under the
/// user is exactly the muscle-memory break D5 exists to prevent. Two keys,
/// always, is a promise that can be kept.
pub const MIN_LEN: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alphabet {
    chars: Vec<char>,
}

impl Default for Alphabet {
    fn default() -> Self {
        Self::new(HOME_ROW).expect("the default alphabet is valid")
    }
}

impl Alphabet {
    /// The sixty ranks of [`RANK_ROWS`], for counting down a column.
    pub fn ranks() -> Self {
        Self::new(RANK_ROWS).expect("the rank alphabet is valid")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AlphabetError {
    /// Fewer than two characters cannot produce a growing label set.
    TooShort,
    /// A repeated character makes a label ambiguous the moment it is typed.
    Duplicate(char),
}

impl std::fmt::Display for AlphabetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "an alphabet needs at least 2 characters"),
            Self::Duplicate(c) => write!(f, "alphabet contains '{c}' twice"),
        }
    }
}

impl std::error::Error for AlphabetError {}

impl Alphabet {
    pub fn new(chars: &str) -> Result<Self, AlphabetError> {
        let chars: Vec<char> = chars.chars().collect();
        if chars.len() < 2 {
            return Err(AlphabetError::TooShort);
        }
        for (i, c) in chars.iter().enumerate() {
            if chars[..i].contains(c) {
                return Err(AlphabetError::Duplicate(*c));
            }
        }
        Ok(Self { chars })
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    /// Position of `c` in the alphabet, or `None` if it is not in it — which
    /// is how a typo is told from a keystroke.
    pub fn index_of(&self, c: char) -> Option<usize> {
        self.chars.iter().position(|k| *k == c)
    }

    /// How many characters every label needs, for `n` targets.
    ///
    /// One length for the whole set, so the set is prefix-free by
    /// construction and the user always types the same number of keys.
    pub fn label_len(&self, n: usize) -> usize {
        let mut len = MIN_LEN;
        let mut capacity = self.len().pow(MIN_LEN as u32);
        while capacity < n {
            capacity = capacity.saturating_mul(self.len());
            len += 1;
        }
        len
    }

    /// How many targets `len` characters can label.
    pub fn capacity(&self, len: usize) -> usize {
        self.len().saturating_pow(len as u32)
    }

    /// The `i`th label of length `len`, counted like an odometer: `aa`, `as`,
    /// `ad`, … so that the first target on screen gets the first label, and
    /// labels sharing a first key are adjacent in reading order.
    fn nth(&self, mut i: usize, len: usize) -> String {
        let mut out = vec![self.chars[0]; len];
        for slot in (0..len).rev() {
            out[slot] = self.chars[i % self.len()];
            i /= self.len();
        }
        out.into_iter().collect()
    }
}

/// Reading order: rows top to bottom, and within a row, left to right.
///
/// Returns indices into `points`, so the caller keeps its own target type.
/// Ties are broken by the original index, which makes the result a total
/// order — two targets at exactly the same point still label deterministically
/// rather than by whatever the sort happened to do.
pub fn reading_order(points: &[Point]) -> Vec<usize> {
    let mut by_y: Vec<usize> = (0..points.len()).collect();
    by_y.sort_by_key(|&i| (points[i].y, points[i].x, i));

    let mut out = Vec::with_capacity(points.len());
    let mut row: Vec<usize> = Vec::new();
    let mut row_y = 0;

    for i in by_y {
        // Grown from the row's first member, never from a fixed grid: that is
        // what stops a one-pixel difference deciding which row a target is in.
        if row.is_empty() || points[i].y - row_y <= ROW_BAND {
            if row.is_empty() {
                row_y = points[i].y;
            }
            row.push(i);
        } else {
            flush_row(&mut row, points, &mut out);
            row_y = points[i].y;
            row.push(i);
        }
    }
    flush_row(&mut row, points, &mut out);
    out
}

fn flush_row(row: &mut Vec<usize>, points: &[Point], out: &mut Vec<usize>) {
    row.sort_by_key(|&i| (points[i].x, points[i].y, i));
    out.append(row);
}

/// Which of `n` columns across `x0 .. x0 + width` the coordinate `x` is in.
///
/// Clamped at both ends, so a target hanging off the edge of the span — a
/// window straddling two monitors, a negative coordinate from a toolkit
/// having a bad day — lands in the nearest column instead of panicking.
fn column_of(x: i32, x0: i32, width: i32, n: usize) -> usize {
    if width <= 0 || n == 0 {
        return 0;
    }
    let rel = i64::from(x - x0).clamp(0, i64::from(width) - 1);
    ((rel * n as i64) / i64::from(width)) as usize
}

/// One label set, and the trie that consumes keystrokes for it.
#[derive(Debug, Clone)]
pub struct Labels {
    /// What the first key names: the column (§5.3b).
    alphabet: Alphabet,
    /// What the keys after it count with: the rank inside that column
    /// (§5.3c). Equal to `alphabet` for the odometer of `assign`.
    ranks: Alphabet,
    len: usize,
    /// `labels[slot]` is the label of `targets[order[slot]]`.
    labels: Vec<String>,
    /// Target index for each label slot, in reading order.
    order: Vec<usize>,
}

impl Labels {
    /// Assign labels to targets by their centres (D5).
    pub fn assign(alphabet: Alphabet, points: &[Point]) -> Self {
        let order = reading_order(points);
        let len = alphabet.label_len(order.len());
        let labels = (0..order.len()).map(|i| alphabet.nth(i, len)).collect();
        let ranks = alphabet.clone();
        Self { alphabet, ranks, len, labels, order }
    }

    /// Assign labels by **screen column** (D5, §5.3b).
    ///
    /// The span `x0 .. x0 + width` is cut into one column per alphabet
    /// character, so the first key names *where the target is* — leftmost
    /// column `a`, rightmost `;` — and the second counts down that column
    /// from the top.
    ///
    /// This is what makes the first keystroke worth pressing: with the
    /// odometer (`assign`) every label of a small set began `a` and the key
    /// discriminated nothing. It is also *more* stable than the odometer,
    /// not less, which is the whole of D5: a target's first key depends only
    /// on its own position, so an unrelated element appearing elsewhere on
    /// screen cannot move it, and its second key moves only if something
    /// appears above it in its own column.
    ///
    /// Columns are allowed to be empty. That is the point — the rightmost
    /// target is `;`-something whether or not anything sits in the middle of
    /// the screen, so the label follows the eye rather than the population.
    pub fn in_columns(
        alphabet: Alphabet,
        ranks: Alphabet,
        points: &[Point],
        x0: i32,
        width: i32,
    ) -> Self {
        let n = alphabet.len();
        let mut columns: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, p) in points.iter().enumerate() {
            columns[column_of(p.x, x0, width, n)].push(i);
        }
        // Down the column, then left to right, then by index: a total order,
        // for the same reason `reading_order` insists on one.
        for column in columns.iter_mut() {
            column.sort_by_key(|&i| (points[i].y, points[i].x, i));
        }

        // One length for the whole set (§5.3), counted from the busiest
        // column: one key names the column, the rest count within it. Ten
        // per column is the common case and costs two keys.
        let busiest = columns.iter().map(Vec::len).max().unwrap_or(0);
        let mut tail = 1;
        let mut capacity = ranks.len();
        while capacity < busiest {
            capacity = capacity.saturating_mul(ranks.len());
            tail += 1;
        }

        let mut labels = Vec::with_capacity(points.len());
        let mut order = Vec::with_capacity(points.len());
        for (c, column) in columns.iter().enumerate() {
            for (k, &target) in column.iter().enumerate() {
                labels.push(format!("{}{}", alphabet.chars()[c], ranks.nth(k, tail)));
                order.push(target);
            }
        }
        Self { alphabet, ranks, len: 1 + tail, labels, order }
    }

    /// Every label with the target index it points at, in reading order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, usize)> {
        self.labels.iter().map(String::as_str).zip(self.order.iter().copied())
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Characters in every label of this set.
    pub fn label_len(&self) -> usize {
        self.len
    }

    /// What the first key names: one character per column.
    pub fn alphabet(&self) -> &Alphabet {
        &self.alphabet
    }

    /// What the keys after the first count with, inside a column.
    pub fn ranks(&self) -> &Alphabet {
        &self.ranks
    }

    /// The label for a target index, if it has one.
    pub fn label_of(&self, target: usize) -> Option<&str> {
        self.order.iter().position(|&t| t == target).map(|slot| self.labels[slot].as_str())
    }

    /// The target a complete label points at.
    pub fn target_of(&self, label: &str) -> Option<usize> {
        self.labels.iter().position(|l| l == label).map(|slot| self.order[slot])
    }

    /// Start typing.
    pub fn typing(&self) -> Typing<'_> {
        Typing { labels: self, typed: String::with_capacity(self.len), matches: self.len() }
    }
}

/// What a keystroke did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// A prefix of `n` labels. Keep the overlay up and dim the rest.
    Pending { matches: usize },
    /// A complete label. `target` indexes the caller's target list.
    Hit { target: usize },
    /// Not in the alphabet, or no label starts this way.
    ///
    /// **Never a click.** §4.5's rule — a wrong click is the only truly
    /// unacceptable outcome — starts here, at the keyboard, before anything
    /// has been validated or injected.
    Miss,
}

/// A typing session over one label set.
///
/// Holds a borrow of the labels rather than a copy: a hint session cannot
/// outlive the snapshot it was drawn from, and the borrow checker saying so
/// is cheaper than discovering it at a click.
#[derive(Debug)]
pub struct Typing<'a> {
    labels: &'a Labels,
    typed: String,
    matches: usize,
}

impl Typing<'_> {
    /// What has been typed so far.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    pub fn matches(&self) -> usize {
        self.matches
    }

    /// Labels still reachable, with their target indices — what the overlay
    /// keeps drawing after each keystroke.
    pub fn candidates(&self) -> impl Iterator<Item = (&str, usize)> {
        let typed = self.typed.clone();
        self.labels.iter().filter(move |(l, _)| l.starts_with(&typed))
    }

    /// Feed one key.
    pub fn press(&mut self, c: char) -> Progress {
        // Both alphabets: the first key comes from the columns, every key
        // after it from the ranks (§5.3c), and a character in neither is a
        // typo rather than a wrong guess.
        if self.labels.alphabet.index_of(c).is_none() && self.labels.ranks.index_of(c).is_none() {
            return Progress::Miss;
        }
        let mut candidate = self.typed.clone();
        candidate.push(c);

        // One pass over a set that is at most a few hundred short strings,
        // and only on a keystroke — the "prefix trie" of §7 without the
        // pointer chasing. If a label set ever gets big enough for this to
        // matter, the shape of the data (fixed-length, dense, in order) makes
        // it a range lookup rather than a rewrite.
        let matches = self.labels.labels.iter().filter(|l| l.starts_with(&candidate)).count();
        if matches == 0 {
            return Progress::Miss;
        }
        self.typed = candidate;
        self.matches = matches;

        if self.typed.chars().count() == self.labels.len {
            match self.labels.target_of(&self.typed) {
                Some(target) => Progress::Hit { target },
                // Unreachable while every label has the same length, and a
                // Miss rather than a panic if that ever stops being true.
                None => Progress::Miss,
            }
        } else {
            Progress::Pending { matches }
        }
    }

    /// Undo the last keystroke.
    pub fn backspace(&mut self) -> Progress {
        self.typed.pop();
        self.matches = self.candidates().count();
        Progress::Pending { matches: self.matches }
    }

    pub fn reset(&mut self) {
        self.typed.clear();
        self.matches = self.labels.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(list: &[(i32, i32)]) -> Vec<Point> {
        list.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    #[test]
    fn reading_order_is_rows_then_columns() {
        // Two rows of three, given in scrambled order.
        let p = points(&[(300, 100), (100, 10), (300, 10), (100, 100), (200, 10), (200, 100)]);
        let order = reading_order(&p);
        let xs: Vec<(i32, i32)> = order.iter().map(|&i| (p[i].x, p[i].y)).collect();
        assert_eq!(xs, vec![(100, 10), (200, 10), (300, 10), (100, 100), (200, 100), (300, 100)]);
    }

    /// gnome-terminal's four header buttons sit at y=0, y=0, y=0 and y=8.
    /// A strict sort by y reads that as two rows and labels them in a column
    /// order the user does not see.
    #[test]
    fn a_row_that_is_not_pixel_aligned_is_still_one_row() {
        let p = points(&[(1880, 8), (6, 0), (1789, 0), (1831, 0)]);
        let order = reading_order(&p);
        let xs: Vec<i32> = order.iter().map(|&i| p[i].x).collect();
        assert_eq!(xs, vec![6, 1789, 1831, 1880]);
    }

    #[test]
    fn rows_further_apart_than_the_band_stay_separate() {
        let p = points(&[(500, 0), (10, ROW_BAND + 1)]);
        let order = reading_order(&p);
        assert_eq!(order, vec![0, 1], "the lower-left target must not jump ahead of the upper-right one");
    }

    /// M3's exit criterion: the same tree gives the same labels. Feeding the
    /// same targets in a different order is the strongest form of that —
    /// detection's own ordering (a `HashMap` walk, in the mirror's case) must
    /// not reach the user.
    #[test]
    fn same_tree_same_labels_whatever_order_it_arrives_in() {
        let a = points(&[(10, 10), (200, 10), (10, 90), (200, 90), (95, 300)]);
        let shuffled = points(&[(95, 300), (200, 90), (10, 10), (10, 90), (200, 10)]);

        let la = Labels::assign(Alphabet::default(), &a);
        let lb = Labels::assign(Alphabet::default(), &shuffled);

        for (i, point) in a.iter().enumerate() {
            let j = shuffled.iter().position(|q| q == point).expect("same set");
            assert_eq!(
                la.label_of(i),
                lb.label_of(j),
                "{point:?} must keep its label however the list was ordered"
            );
        }
    }

    /// gnome-terminal's header bar, as detection actually reports it, with
    /// the window maximized on a 1920-wide screen. These are the numbers
    /// M5's first click was aimed with.
    #[test]
    fn columns_name_where_a_target_is() {
        // centres of: New Tab, Find, Menu, Close, and the two page tabs
        let p = [
            Point::new(24, 23),    // 6,0 36x46
            Point::new(1807, 23),  // 1789,0 36x46
            Point::new(1849, 23),  // 1831,0 36x46
            Point::new(1897, 23),  // 1880,8 34x30
            Point::new(471, 67),   // 379,50 184x34
            Point::new(1405, 67),  // 1205,50 400x34
        ];
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 1920);

        assert_eq!(l.label_len(), 2, "ten per column fits in two keys");
        assert_eq!(l.label_of(0), Some("aa"), "leftmost column, first down it");
        // Find, Menu and Close all sit in the last 192 px, so they share the
        // ';' column and are counted left to right — they are level.
        assert_eq!(l.label_of(1), Some(";a"));
        assert_eq!(l.label_of(2), Some(";s"));
        assert_eq!(l.label_of(3), Some(";d"));
        // The tabs land where the eye puts them: a quarter and three
        // quarters across.
        assert_eq!(l.label_of(4), Some("da"), "471/1920 -> column 2");
        assert_eq!(l.label_of(5), Some("ka"), "1405/1920 -> column 7");
    }

    /// The property the odometer could not offer, and the reason for the
    /// change: a new target elsewhere on screen must not rename anything.
    #[test]
    fn a_new_target_elsewhere_does_not_move_a_label() {
        let alphabet = Alphabet::default();
        // The new target appears *between* the two, on the same row — the
        // case reading order is most sensitive to.
        let before = [Point::new(24, 23), Point::new(1849, 23)];
        let after = [Point::new(24, 23), Point::new(1849, 23), Point::new(500, 23)];

        let a = Labels::in_columns(alphabet.clone(), Alphabet::ranks(), &before, 0, 1920);
        let b = Labels::in_columns(alphabet.clone(), Alphabet::ranks(), &after, 0, 1920);
        assert_eq!(a.label_of(0), b.label_of(0));
        assert_eq!(a.label_of(1), b.label_of(1));

        // Where the odometer renames the second target the moment a target
        // is inserted before it in reading order.
        let c = Labels::assign(alphabet.clone(), &before);
        let d = Labels::assign(alphabet, &after);
        assert_eq!(c.label_of(1), Some("as"));
        assert_eq!(d.label_of(1), Some("ad"), "the odometer moved it");
    }

    /// The rank rows in order: home row, then top, then bottom, then the
    /// same three shifted (§5.3c).
    #[test]
    fn a_crowded_column_walks_out_to_the_other_rows() {
        let p: Vec<Point> = (0..32).map(|i| Point::new(100, i * 40)).collect();
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 1920);

        assert_eq!(l.label_len(), 2, "sixty ranks is still one key");
        assert_eq!(l.label_of(0), Some("aa"), "home row first");
        assert_eq!(l.label_of(9), Some("a;"), "…to the end of it");
        assert_eq!(l.label_of(10), Some("aq"), "then the top row");
        assert_eq!(l.label_of(19), Some("ap"));
        assert_eq!(l.label_of(20), Some("az"), "then the bottom row");
        assert_eq!(l.label_of(29), Some("a/"));
        assert_eq!(l.label_of(30), Some("aA"), "then shifted, home row again");
        assert_eq!(l.label_of(31), Some("aS"));
    }

    #[test]
    fn a_column_deeper_than_the_ranks_lengthens_every_label() {
        // Sixty-one in one column: the tail needs two keys, and one length
        // for the whole set means every label gets three.
        let p: Vec<Point> = (0..61).map(|i| Point::new(100, i * 40)).collect();
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 1920);
        assert_eq!(l.label_len(), 3);
        assert_eq!(l.label_of(0), Some("aaa"));
        assert_eq!(l.label_of(60), Some("asa"));
        // Still prefix-free: no label is the start of another.
        let all: Vec<&str> = l.iter().map(|(s, _)| s).collect();
        for a in &all {
            assert_eq!(all.iter().filter(|b| b.starts_with(*a)).count(), 1);
        }
    }

    /// A capital is a keystroke like any other — the shell sends the unicode
    /// the keysym produced, so Shift is the user's business and not muvor's.
    #[test]
    fn a_shifted_rank_is_typed_like_any_other() {
        let p: Vec<Point> = (0..31).map(|i| Point::new(100, i * 40)).collect();
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 1920);
        let mut t = l.typing();
        assert!(matches!(t.press('a'), Progress::Pending { .. }));
        assert_eq!(t.press('A'), Progress::Hit { target: 30 });

        // And a character in neither alphabet is still a typo, not a click.
        let mut t = l.typing();
        assert_eq!(t.press('§'), Progress::Miss);
        assert_eq!(t.typed(), "");
    }

    #[test]
    fn a_target_outside_the_span_lands_in_the_nearest_column() {
        let p = [Point::new(-40, 10), Point::new(5000, 10)];
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 1920);
        assert_eq!(l.label_of(0), Some("aa"));
        assert_eq!(l.label_of(1), Some(";a"));
    }

    #[test]
    fn a_degenerate_span_does_not_divide_by_zero() {
        let p = [Point::new(10, 10), Point::new(20, 20)];
        let l = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &p, 0, 0);
        assert_eq!(l.label_of(0), Some("aa"));
        assert_eq!(l.label_of(1), Some("as"));
    }

    #[test]
    fn labels_are_handed_out_in_reading_order() {
        let p = points(&[(200, 10), (10, 10), (10, 90)]);
        let l = Labels::assign(Alphabet::default(), &p);
        assert_eq!(l.label_of(1), Some("aa"));
        assert_eq!(l.label_of(0), Some("as"));
        assert_eq!(l.label_of(2), Some("ad"));
    }

    #[test]
    fn every_label_is_the_same_length_and_they_are_all_distinct() {
        for n in [1usize, 4, 33, 99, 100, 101, 500] {
            let p: Vec<Point> = (0..n).map(|i| Point::new((i % 20) as i32 * 40, (i / 20) as i32 * 40)).collect();
            let l = Labels::assign(Alphabet::default(), &p);
            let expect = if n <= 100 { 2 } else { 3 };
            assert_eq!(l.label_len(), expect, "{n} targets");
            let mut all: Vec<&str> = l.iter().map(|(s, _)| s).collect();
            all.sort_unstable();
            let before = all.len();
            all.dedup();
            assert_eq!(all.len(), before, "{n} targets produced a duplicate label");
            assert!(all.iter().all(|s| s.chars().count() == expect));
        }
    }

    /// Same length for every label is what makes the set prefix-free, which
    /// is what makes a completed label unambiguous. Worth asserting rather
    /// than assuming, since it is the property the typing state machine
    /// rests on.
    #[test]
    fn no_label_is_a_prefix_of_another() {
        let p: Vec<Point> = (0..64).map(|i| Point::new((i % 8) * 40, (i / 8) * 40)).collect();
        let l = Labels::assign(Alphabet::default(), &p);
        let all: Vec<&str> = l.iter().map(|(s, _)| s).collect();
        for a in &all {
            for b in &all {
                assert!(a == b || !b.starts_with(a), "{a} is a prefix of {b}");
            }
        }
    }

    #[test]
    fn typing_a_label_hits_its_target() {
        let p = points(&[(10, 10), (200, 10), (10, 90)]);
        let l = Labels::assign(Alphabet::default(), &p);
        let mut t = l.typing();
        assert_eq!(t.press('a'), Progress::Pending { matches: 3 });
        assert_eq!(t.press('d'), Progress::Hit { target: 2 });
    }

    #[test]
    fn a_key_outside_the_alphabet_is_a_miss_and_changes_nothing() {
        let p = points(&[(10, 10), (200, 10)]);
        let l = Labels::assign(Alphabet::default(), &p);
        let mut t = l.typing();
        assert_eq!(t.press('q'), Progress::Miss);
        assert_eq!(t.typed(), "");
        assert_eq!(t.press('a'), Progress::Pending { matches: 2 });
    }

    /// An alphabet character that no label starts with is still a miss — and
    /// it must not consume the keystroke, or the next key would be matched
    /// against a prefix that never existed.
    #[test]
    fn a_valid_character_with_no_label_behind_it_is_a_miss() {
        let p = points(&[(10, 10), (200, 10)]);
        let l = Labels::assign(Alphabet::default(), &p);
        let mut t = l.typing();
        assert_eq!(t.press('s'), Progress::Miss, "only 'aa' and 'as' exist");
        assert_eq!(t.typed(), "");
    }

    #[test]
    fn backspace_returns_to_the_previous_prefix() {
        let p: Vec<Point> = (0..30).map(|i| Point::new((i % 6) * 40, (i / 6) * 40)).collect();
        let l = Labels::assign(Alphabet::default(), &p);
        let mut t = l.typing();
        t.press('a');
        assert_eq!(t.matches(), 10);
        t.press('d');
        assert_eq!(t.matches(), 1);
        t.backspace();
        assert_eq!(t.typed(), "a");
        assert_eq!(t.matches(), 10);
    }

    #[test]
    fn candidates_narrow_as_keys_arrive() {
        let p: Vec<Point> = (0..25).map(|i| Point::new((i % 5) * 40, (i / 5) * 40)).collect();
        let l = Labels::assign(Alphabet::default(), &p);
        let mut t = l.typing();
        assert_eq!(t.candidates().count(), 25);
        t.press('a');
        assert_eq!(t.candidates().count(), 10);
        assert!(t.candidates().all(|(label, _)| label.starts_with('a')));
    }

    #[test]
    fn an_alphabet_must_be_usable() {
        assert_eq!(Alphabet::new("a"), Err(AlphabetError::TooShort));
        assert_eq!(Alphabet::new("aba"), Err(AlphabetError::Duplicate('a')));
        assert!(Alphabet::new("asdf").is_ok());
    }

    #[test]
    fn a_smaller_alphabet_just_means_longer_labels() {
        let alphabet = Alphabet::new("ab").expect("valid");
        let p: Vec<Point> = (0..8).map(|i| Point::new(0, i * 40)).collect();
        let l = Labels::assign(alphabet, &p);
        assert_eq!(l.label_len(), 3);
        assert_eq!(l.label_of(0), Some("aaa"));
        assert_eq!(l.label_of(7), Some("bbb"));
    }

    #[test]
    fn no_targets_is_not_a_panic() {
        let l = Labels::assign(Alphabet::default(), &[]);
        assert!(l.is_empty());
        let mut t = l.typing();
        assert_eq!(t.press('a'), Progress::Miss);
    }
}
