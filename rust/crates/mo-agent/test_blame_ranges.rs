fn main() {
    // Test if BlameRanges::from_one_based_inclusive_ranges handles inverted ranges (5..=3)
    
    // Try with inverted range
    match gix::blame::BlameRanges::from_one_based_inclusive_ranges(
        vec![5u32..=3u32],
    ) {
        Ok(ranges) => println!("Inverted range (5..=3) accepted: {:?}", ranges),
        Err(e) => println!("Inverted range (5..=3) error: {}", e),
    }
    
    // Try with normal range
    match gix::blame::BlameRanges::from_one_based_inclusive_ranges(
        vec![3u32..=5u32],
    ) {
        Ok(ranges) => println!("Normal range (3..=5) accepted: {:?}", ranges),
        Err(e) => println!("Normal range (3..=5) error: {}", e),
    }
}
