#[must_use]
pub const fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }

    proptest! {
        #[test]
        // Debug builds panic on u64 overflow, so each operand stays within u64::MAX / 2.
        fn add_agrees_with_u64_addition(left in 0..=u64::MAX / 2, right in 0..=u64::MAX / 2) {
            prop_assert_eq!(add(left, right), left + right);
        }
    }
}
