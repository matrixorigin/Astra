/// Stable polyfill for the `round_char_boundary` nightly feature (issue #93743).
///
/// Provides `floor_char_boundary` and `ceil_char_boundary` as extension methods
/// on `str`, matching the semantics of the unstable inherent methods so call
/// sites can be updated transparently.
pub trait CharBoundaryExt {
    /// Returns the largest byte index `<= index` that is a valid UTF-8 char boundary.
    fn floor_char_boundary(&self, index: usize) -> usize;
    /// Returns the smallest byte index `>= index` that is a valid UTF-8 char boundary.
    fn ceil_char_boundary(&self, index: usize) -> usize;
}

impl CharBoundaryExt for str {
    fn floor_char_boundary(&self, index: usize) -> usize {
        let index = index.min(self.len());
        (0..=index)
            .rev()
            .find(|&i| self.is_char_boundary(i))
            .unwrap_or(0)
    }

    fn ceil_char_boundary(&self, index: usize) -> usize {
        if index >= self.len() {
            return self.len();
        }
        (index..=self.len())
            .find(|&i| self.is_char_boundary(i))
            .unwrap_or(self.len())
    }
}
