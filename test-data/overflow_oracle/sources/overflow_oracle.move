module overflow_oracle::math_u256_oracle;

use std::string::{Self as string, String};
use movy::oracle;
use overflow::math_u256;

const MAX_U256: u256 = 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff;

/// Oracle for `div_mod`.
///
/// Verifies three properties:
/// 1.  **Pre-condition**: The denominator `denom` cannot be zero.
/// 2.  **Post-condition**: The remainder `r` must be strictly less than the denominator `denom`.
/// 3.  **Identity**: The division identity `num == (p * denom) + r` must hold true.
public fun div_mod_wrapper(num: u256, denom: u256): (u256, u256) {
    if (denom == 0) {
        oracle::crash_because(string::utf8(b"Pre-condition failed: Division by zero"));
    };

    let (p, r) = math_u256::div_mod(num, denom);

    if (r >= denom) {
        oracle::crash_because(string::utf8(b"Post-condition failed: Remainder is not less than denominator"));
    };

    // The property p = num / denom guarantees p * denom <= num, so (p * denom) + r will not overflow
    // if num itself did not. The check is safe.
    if (num != (p * denom) + r) {
         oracle::crash_because(string::utf8(b"Identity failed: num != p * denom + r"));
    };

    (p, r)
}

/// Oracle for `shlw`.
///
/// Verifies that a left shift by 64 is equivalent to multiplication by 2^192 in the non-overflow case.
/// 1.  Calculates the result `res` from the target function.
/// 2.  Checks if an overflow (loss of bits) would occur. This happens if any of the top 64 bits of `n` are set.
/// 3.  If no overflow occurs, it verifies that `(res >> 64) == n`.
public fun shlw_wrapper(n: u256): u256 {
    let res = math_u256::shlw(n);

    // If the top 64 bits of n are zero, no information is lost.
    // The inverse operation (right shift) should restore the original number.
    let no_overflow = (n >> 192) == 0;

    if (no_overflow) {
        if ((res >> 64) != n) {
            oracle::crash_because(string::utf8(b"Shift result is incorrect in non-overflow case"));
        }
    } else {
        // If an overflow did occur, the inverse operation will not yield the original number.
        if ((res >> 64) == n) {
            oracle::crash_because(string::utf8(b"Shift result implies no overflow when there should be"));
        }
    };

    res
}

/// Oracle for `shrw`.
///
/// Verifies that a right shift by 64 is mathematically equivalent to integer division by 2^64.
public fun shrw_wrapper(n: u256): u256 {
    let res = math_u256::shrw(n);

    let two_pow_64 = (1 as u256) << 64;
    if (res != (n / two_pow_64)) {
        oracle::crash_because(string::utf8(b"Right shift result does not match division by 2^64"));
    };

    res
}

/// Oracle for `checked_shlw`.
///
/// This oracle finds a bug in the original implementation. The original code uses `n > mask`, which
/// incorrectly identifies overflow conditions.
/// 1.  It determines the correct overflow condition: `(n >> 192) != 0`.
/// 2.  It asserts that the reported overflow flag matches the correct condition.
/// 3.  It verifies that the result is `0` on overflow and `n << 64` otherwise.
public fun checked_shlw_wrapper(n: u256): (u256, bool) {
    let (res, has_overflow) = math_u256::checked_shlw(n);

    // The correct condition for overflow is if any of the top 64 bits are set.
    let expected_overflow = (n >> 192) != 0;

    if (has_overflow != expected_overflow) {
        oracle::crash_because(string::utf8(b"Incorrect overflow flag reported"));
    };

    if (expected_overflow) {
        if (res != 0) {
            oracle::crash_because(string::utf8(b"Result should be 0 on overflow"));
        }
    } else {
        if (res != (n << 64)) {
            oracle::crash_because(string::utf8(b"Result is incorrect in non-overflow case"));
        }
    };

    (res, has_overflow)
}

/// Oracle for `div_round`.
///
/// Verifies the rounding logic.
/// 1.  **Pre-condition**: Checks for division by zero.
/// 2.  Calculates the expected result based on the quotient, remainder, and `round_up` flag.
/// 3.  **Pre-condition**: Predicts and flags cases where rounding up `p + 1` would cause an overflow.
/// 4.  Asserts that the function's result matches the expected result.
public fun div_round_wrapper(num: u256, denom: u256, round_up: bool): u256 {
    if (denom == 0) {
        oracle::crash_because(string::utf8(b"Pre-condition failed: Division by zero"));
    };

    let res = math_u256::div_round(num, denom, round_up);

    let p = num / denom;
    let r = num % denom;

    let expected_res: u256;
    if (round_up && r != 0) {
        // Check if p + 1 would overflow. If so, the original function would abort.
        if (p == MAX_U256) {
            oracle::crash_because(string::utf8(b"Pre-condition failed: Rounding up causes overflow"));
        };
        expected_res = p + 1;
    } else {
        expected_res = p;
    };

    if (res != expected_res) {
        oracle::crash_because(string::utf8(b"Incorrect rounded division result"));
    };

    res
}

/// Oracle for `add_check`.
///
/// Verifies the commutative property of the addition check. The check for `a + b` should yield
/// the same result as the check for `b + a`.
public fun add_check_wrapper(num1: u256, num2: u256): bool {
    let res = math_u256::add_check(num1, num2);

    // Property: The check should be commutative.
    if (res != math_u256::add_check(num2, num1)) {
        oracle::crash_because(string::utf8(b"Property failed: Add check is not commutative"));
    };

    res
}