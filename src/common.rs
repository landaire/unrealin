pub fn normalize_index(index: i32) -> usize {
    match index {
        i if i < 0 => (-index) as usize - 1,
        i if i > 0 => index as usize - 1,
        _ => 0,
    }
}
