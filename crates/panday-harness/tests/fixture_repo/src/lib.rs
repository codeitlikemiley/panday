//! A deliberately broken crate, used as the target of the M13.2 loop test.
//!
//! `sum_to` is off by one: it excludes `n`. The test below states the correct
//! behaviour, so `cargo test` fails until the bug is fixed — which is the
//! point. Do not "fix" this file; the harness test is what repairs it, in a
//! copy, and asserts that it did.

/// Sum every integer from 1 to `n`, inclusive.
pub fn sum_to(n: u32) -> u32 {
    (1..n).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_inclusively() {
        assert_eq!(sum_to(5), 15, "1+2+3+4+5");
    }
}
