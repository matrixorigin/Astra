//! Shared normalization for inclusive line ranges.

/// Order two explicitly supplied positive bounds; leave missing or invalid
/// bounds alone so callers retain their validation semantics.
pub fn ordered_bounds<T: Copy + Ord + Default>(
    start: Option<T>,
    end: Option<T>,
) -> (Option<T>, Option<T>) {
    match (start, end) {
        (Some(start), Some(end)) if end > T::default() && start > end => (Some(end), Some(start)),
        bounds => bounds,
    }
}
