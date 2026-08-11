//! koe-ffi — uniffi-generated bindings and type conversions.

uniffi::setup_scaffolding!();

/// Smoke-test export used to verify uniffi Swift binding generation.
#[uniffi::export]
#[must_use]
pub const fn add(
    left: u64,
    right: u64,
) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
