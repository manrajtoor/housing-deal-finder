//! Medians and percentiles. Medians, never means: one mistyped price drags a
//! mean badly and barely moves a median.
//!
//! The scorer keeps each comp group's values sorted once and asks for
//! statistics "without the subject" through [`Sorted::without`], which skips
//! one element by index instead of copying the group for every listing.

/// Sorts a copy of `values` ascending (NaN-free input assumed; NaNs sort last).
pub fn sorted(values: &[f64]) -> Vec<f64> {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Greater));
    v
}

/// A sorted slice with at most one element left out.
#[derive(Debug, Clone, Copy)]
pub struct Sorted<'a> {
    values: &'a [f64],
    skip: Option<usize>,
}

impl<'a> Sorted<'a> {
    /// `values` must already be sorted ascending.
    pub fn new(values: &'a [f64]) -> Sorted<'a> {
        Sorted { values, skip: None }
    }

    /// The same values without one occurrence of `v` (no-op when absent).
    /// Equal values are interchangeable, so any matching index will do.
    pub fn without(values: &'a [f64], v: f64) -> Sorted<'a> {
        let skip = values
            .binary_search_by(|x| x.partial_cmp(&v).unwrap_or(std::cmp::Ordering::Less))
            .ok();
        Sorted { values, skip }
    }

    pub fn len(&self) -> usize {
        self.values.len() - usize::from(self.skip.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, i: usize) -> f64 {
        match self.skip {
            Some(s) if i >= s => self.values[i + 1],
            _ => self.values[i],
        }
    }

    pub fn median(&self) -> Option<f64> {
        let n = self.len();
        if n == 0 {
            return None;
        }
        Some(if n % 2 == 1 { self.get(n / 2) } else { (self.get(n / 2 - 1) + self.get(n / 2)) / 2.0 })
    }

    /// Linearly interpolated percentile (the "type 7" rule of R and numpy).
    pub fn percentile(&self, p: f64) -> Option<f64> {
        let n = self.len();
        if n == 0 {
            return None;
        }
        let pos = (p.clamp(0.0, 100.0) / 100.0) * (n - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        let (a, b) = (self.get(lo), self.get(hi));
        Some(a + (b - a) * (pos - lo as f64))
    }
}

pub fn median(values: &[f64]) -> Option<f64> {
    Sorted::new(&sorted(values)).median()
}

pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    Sorted::new(&sorted(values)).percentile(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_ignores_an_outlier() {
        assert_eq!(median(&[500_000.0, 510_000.0, 490_000.0, 505_000.0, 5_000.0]), Some(500_000.0));
    }

    #[test]
    fn percentiles_interpolate() {
        let v = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile(&v, 25.0), Some(20.0));
        assert_eq!(percentile(&v, 75.0), Some(40.0));
        assert_eq!(percentile(&[10.0, 20.0], 50.0), Some(15.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn without_skips_exactly_one_occurrence() {
        let v = sorted(&[1.0, 2.0, 2.0, 3.0, 100.0]);
        let s = Sorted::without(&v, 100.0);
        assert_eq!(s.len(), 4);
        assert_eq!(s.median(), Some(2.0));
        let s = Sorted::without(&v, 2.0);
        assert_eq!(s.len(), 4);
        assert_eq!(s.median(), Some(2.5), "one 2 left: 1, 2, 3, 100");
        let s = Sorted::without(&v, 7.0);
        assert_eq!(s.len(), 5, "absent value: nothing skipped");
        let s = Sorted::without(&v, 1.0);
        assert_eq!((s.percentile(0.0), s.percentile(100.0)), (Some(2.0), Some(100.0)));
    }
}
