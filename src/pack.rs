//! MaxRects bin packing, spread across as many bins as needed.

const EPS: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn right(&self) -> f64 {
        self.x + self.w
    }

    fn top(&self) -> f64 {
        self.y + self.h
    }

    fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() - EPS
            && o.x < self.right() - EPS
            && self.y < o.top() - EPS
            && o.y < self.top() - EPS
    }

    fn contains(&self, o: &Rect) -> bool {
        o.x >= self.x - EPS
            && o.y >= self.y - EPS
            && o.right() <= self.right() + EPS
            && o.top() <= self.top() + EPS
    }
}

/// Where an item ended up. `x`/`y` are measured from the bin's top-left corner,
/// growing right and down. `rotated` means the item is turned 90°, so it
/// occupies `h × w` instead of `w × h`.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub rotated: bool,
}

/// How to choose among the free spots an item fits into.
#[derive(Clone, Copy)]
enum Heuristic {
    /// Topmost, then leftmost spot. Gives tidy rows/grids.
    TopLeft,
    /// Spot whose shorter leftover side is smallest.
    BestShortSide,
    /// Smallest spot.
    BestArea,
}

struct MaxRects {
    free: Vec<Rect>,
    allow_rotate: bool,
    heuristic: Heuristic,
}

impl MaxRects {
    fn new(w: f64, h: f64, allow_rotate: bool, heuristic: Heuristic) -> Self {
        Self {
            free: vec![Rect { x: 0.0, y: 0.0, w, h }],
            allow_rotate,
            heuristic,
        }
    }

    /// Places a `w × h` item, turning it 90° only if it doesn't fit upright.
    fn insert(&mut self, w: f64, h: f64) -> Option<(f64, f64, bool)> {
        if let Some((x, y)) = self.insert_oriented(w, h) {
            return Some((x, y, false));
        }
        if self.allow_rotate && (w - h).abs() > EPS {
            if let Some((x, y)) = self.insert_oriented(h, w) {
                return Some((x, y, true));
            }
        }
        None
    }

    fn insert_oriented(&mut self, w: f64, h: f64) -> Option<(f64, f64)> {
        let mut best: Option<((f64, f64, f64, f64), Rect)> = None;
        for f in &self.free {
            if w > f.w + EPS || h > f.h + EPS {
                continue;
            }
            let (dx, dy) = (f.w - w, f.h - h);
            let score = match self.heuristic {
                Heuristic::TopLeft => (f.y, f.x, 0.0, 0.0),
                Heuristic::BestShortSide => (dx.min(dy), dx.max(dy), f.y, f.x),
                Heuristic::BestArea => (f.w * f.h, dx.min(dy), f.y, f.x),
            };
            if best.is_none_or(|(s, _)| score < s) {
                best = Some((score, Rect { x: f.x, y: f.y, w, h }));
            }
        }
        let (_, used) = best?;
        self.split(&used);
        Some((used.x, used.y))
    }

    fn split(&mut self, used: &Rect) {
        let mut next = Vec::with_capacity(self.free.len() * 2);
        for f in self.free.drain(..) {
            if !f.intersects(used) {
                next.push(f);
                continue;
            }
            if used.x > f.x + EPS {
                next.push(Rect { x: f.x, y: f.y, w: used.x - f.x, h: f.h });
            }
            if used.right() < f.right() - EPS {
                next.push(Rect { x: used.right(), y: f.y, w: f.right() - used.right(), h: f.h });
            }
            if used.y > f.y + EPS {
                next.push(Rect { x: f.x, y: f.y, w: f.w, h: used.y - f.y });
            }
            if used.top() < f.top() - EPS {
                next.push(Rect { x: f.x, y: used.top(), w: f.w, h: f.top() - used.top() });
            }
        }

        // Drop free rects fully contained in another one (keeping one of any duplicates).
        let mut keep = vec![true; next.len()];
        for i in 0..next.len() {
            for j in 0..next.len() {
                if i != j && keep[j] && next[j].contains(&next[i]) && (next[i] != next[j] || i > j) {
                    keep[i] = false;
                    break;
                }
            }
        }
        self.free = next
            .into_iter()
            .zip(keep)
            .filter_map(|(r, k)| k.then_some(r))
            .collect();
    }
}

/// Packs `sizes` (w, h) into bins of `bin_w × bin_h`, leaving at least `gap`
/// between neighbouring items. Every item must fit in an empty bin.
///
/// Each bin is filled greedily; several orderings and placement heuristics
/// are tried and the one covering the most area wins.
pub fn pack(
    sizes: &[(f64, f64)],
    bin_w: f64,
    bin_h: f64,
    gap: f64,
    allow_rotate: bool,
) -> Vec<Vec<Placement>> {
    let by = |key: fn((f64, f64)) -> f64| {
        let mut order: Vec<usize> = (0..sizes.len()).collect();
        // Stable sort, descending: big items first, equal ones keep file order.
        order.sort_by(|&a, &b| key(sizes[b]).total_cmp(&key(sizes[a])));
        order
    };
    let orders = [
        by(|(_, h)| h),
        by(|(w, h)| w.max(h)),
        by(|(w, h)| w * h),
    ];
    // TopLeft first: on ties the tidiest layout wins.
    let heuristics = [Heuristic::TopLeft, Heuristic::BestShortSide, Heuristic::BestArea];

    let mut placed = vec![false; sizes.len()];
    let mut pages = Vec::new();
    while placed.iter().any(|p| !p) {
        let mut best: Option<(f64, Vec<Placement>)> = None;
        for order in &orders {
            for &heuristic in &heuristics {
                let remaining = order.iter().copied().filter(|&i| !placed[i]);
                let page = fill_bin(sizes, remaining, bin_w, bin_h, gap, allow_rotate, heuristic);
                let area: f64 = page.iter().map(|p| sizes[p.index].0 * sizes[p.index].1).sum();
                if best.as_ref().is_none_or(|(a, _)| area > a + EPS) {
                    best = Some((area, page));
                }
            }
        }
        let (_, mut page) = best.unwrap();
        assert!(!page.is_empty(), "item larger than an empty page");
        for p in &page {
            placed[p.index] = true;
        }
        page.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        pages.push(page);
    }
    pages
}

fn fill_bin(
    sizes: &[(f64, f64)],
    items: impl Iterator<Item = usize>,
    bin_w: f64,
    bin_h: f64,
    gap: f64,
    allow_rotate: bool,
    heuristic: Heuristic,
) -> Vec<Placement> {
    // Each item gets `gap` added on its right/bottom; the bin grows by the
    // same amount so the last row/column can still reach the far edge.
    let mut bin = MaxRects::new(bin_w + gap, bin_h + gap, allow_rotate, heuristic);
    items
        .filter_map(|i| {
            let (w, h) = sizes[i];
            let (x, y, rotated) = bin.insert(w + gap, h + gap)?;
            Some(Placement { index: i, x, y, rotated })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(p: &Placement, sizes: &[(f64, f64)]) -> Rect {
        let (w, h) = sizes[p.index];
        let (w, h) = if p.rotated { (h, w) } else { (w, h) };
        Rect { x: p.x, y: p.y, w, h }
    }

    #[test]
    fn nine_cards_fit_on_a4() {
        // Poker cards (63 × 88 mm) on A4 with 10 mm margins: 3 × 3 per page.
        let sizes = vec![(63.0, 88.0); 18];
        let pages = pack(&sizes, 190.0, 277.0, 0.0, true);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), 9);
    }

    #[test]
    fn no_overlaps_and_within_bounds() {
        let sizes: Vec<_> = (0..60)
            .map(|i| (10.0 + (i * 37 % 50) as f64, 10.0 + (i * 53 % 70) as f64))
            .collect();
        let (bw, bh, gap) = (190.0, 277.0, 3.0);
        let pages = pack(&sizes, bw, bh, gap, true);
        let total: usize = pages.iter().map(Vec::len).sum();
        assert_eq!(total, sizes.len());

        for page in &pages {
            let rects: Vec<_> = page.iter().map(|p| placed(p, &sizes)).collect();
            for (i, a) in rects.iter().enumerate() {
                assert!(a.x >= -EPS && a.y >= -EPS);
                assert!(a.right() <= bw + EPS && a.top() <= bh + EPS);
                for b in &rects[i + 1..] {
                    let grown = Rect { x: a.x, y: a.y, w: a.w + gap, h: a.h + gap };
                    let other = Rect { x: b.x, y: b.y, w: b.w + gap, h: b.h + gap };
                    assert!(!grown.intersects(&other), "{a:?} overlaps {b:?}");
                }
            }
        }
    }
}
