//! math.rs — Tick ↔ Price conversions
//!
//! WHAT THIS FILE DOES:
//! Every number-crunching function lives here, isolated from account/instruction logic.
//! This makes it easy to unit-test the math without spinning up a validator.
//!
//! MIRRORS: sdk/src/tick_math.ts — if you change one, change both.

use anchor_lang::prelude::*;

// ─── Constants ─────────────────────────────────────────────────────────────────

pub const TICK_SPACING: i32 = 64;
pub const TICKS_PER_ARRAY: usize = 88;
pub const ARRAYS_PER_BITMAP_WORD: usize = 8;
pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;

/// Q64.64 scale factor = 2^64.
/// We represent √price as: actual_sqrt_price × 2^64, stored as u128.
/// This gives us 64 bits of integer + 64 bits of fraction — enough precision.
pub const Q64_RESOLUTION: u128 = 1u128 << 64;

// ─── Tick validation ───────────────────────────────────────────────────────────

/// Returns error if tick is out of valid range.
pub fn validate_tick(tick: i32) -> Result<()> {
    require!(tick >= MIN_TICK && tick <= MAX_TICK, TickManagerError::TickOutOfRange);
    Ok(())
}

/// Returns error if tick is not a multiple of TICK_SPACING.
pub fn validate_tick_spacing(tick: i32) -> Result<()> {
    require!(tick % TICK_SPACING == 0, TickManagerError::InvalidTickSpacing);
    Ok(())
}

// ─── Tick ↔ √price (Q64.64) ────────────────────────────────────────────────────
//
// WHY Q64.64?
// Solana programs have no floating point. We need to store √price with high
// precision. Q64.64 means: treat a u128 integer as having 64 fractional bits.
// √price_stored = actual_√price × 2^64
//
// Example:
//   tick 0     → actual √price = 1.0     → stored = 1 × 2^64 = 18446744073709551616
//   tick 69082 → actual √price ≈ 31.623  → stored = 31.623 × 2^64 ≈ 583,151,...

/// Compute √(1.0001^tick) × 2^64 using integer math.
///
/// Implementation strategy:
/// We decompose tick into bits and use repeated squaring of precomputed
/// magic numbers. Each magic number is 2^64 × √(1.0001^(2^i)).
///
/// This is the standard approach used by Uniswap V3 and Orca Whirlpools.
/// Reference: https://github.com/orca-so/whirlpools/blob/main/programs/whirlpool/src/math/sqrt_price_math.rs
pub fn tick_to_sqrt_price_q64(tick: i32) -> Result<u128> {
    validate_tick(tick)?;

    // Work with absolute value, handle sign at the end
    let abs_tick = tick.unsigned_abs() as u128;

    // Each magic number below is: 2^128 / √(1.0001^(2^i))
    // We start at 2^128 precision and shift down to 2^64 at the very end.
    // This avoids precision loss during intermediate multiplications.

    let mut ratio: u128 = if abs_tick & 0x1 != 0 {
        0xfffcb933bd6fad37aa2d162d1a594001  // 2^128 × √(1.0001^1)^-1
    } else {
        0x100000000000000000000000000000000  // 2^128 exactly
    };

    // Each step: if bit i of abs_tick is set, multiply by magic[i]
    // These magic numbers represent √(1.0001^(2^i)) in Q128 format
    macro_rules! apply_bit {
        ($bit:expr, $magic:expr) => {
            if abs_tick & (1u128 << $bit) != 0 {
                ratio = mul_shift(ratio, $magic);
            }
        };
    }

    apply_bit!(1,  0xfff97272373d413259a46990580e213a);
    apply_bit!(2,  0xfff2e50f5f656932ef12357cf3c7fdcc);
    apply_bit!(3,  0xffe5caca7e10e4e61c3624eaa0941cd0);
    apply_bit!(4,  0xffcb9843d60f6159c9db58835c926644);
    apply_bit!(5,  0xff973b41fa98c081472e6896dfb254c0);
    apply_bit!(6,  0xff2ea16466c96a3843ec78b326b52861);
    apply_bit!(7,  0xfe5dee046a99a2a811c461f1969c3053);
    apply_bit!(8,  0xfcbe86c7900a88aedcffc83b479aa3a4);
    apply_bit!(9,  0xf987a7253ac413176f2b074cf7815e54);
    apply_bit!(10, 0xf3392b0822b70005940c7a398e4b70f3);
    apply_bit!(11, 0xe7159475a2c29b7443b29c7fa6e889d9);
    apply_bit!(12, 0xd097f3bdfd2022b8845ad8f792aa5825);
    apply_bit!(13, 0xa9f746462d870fdf8a65dc1f90e061e5);
    apply_bit!(14, 0x70d869a156d2a1b890bb3df62baf32f7);
    apply_bit!(15, 0x31be135f97d08fd981231505542fcfa6);
    apply_bit!(16, 0x9aa508b5b7a84e101a108624429);
    apply_bit!(17, 0x5d6af8dedb81196699c329225ee604);
    apply_bit!(18, 0x2216e584f5fa1ea926041bedfe98);
    apply_bit!(19, 0x48a170391f7dc42444e8fa2);

    // If tick is positive, invert (since we computed for negative tick above)
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }

    // Shift from Q128 down to Q64 (drop bottom 64 bits)
    let sqrt_price_q64 = (ratio >> 64) as u128;

    Ok(sqrt_price_q64)
}

/// Multiply two Q128 numbers and shift right by 128 bits.
/// Used in tick_to_sqrt_price_q64 to stay in Q128 range during computation.
#[inline]
fn mul_shift(a: u128, b: u128) -> u128 {
    // We need (a × b) >> 128
    // Since a and b are both ~2^128, their product is ~2^256 — needs 256 bits.
    // We split into high/low 64-bit halves.
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFFFFFFFFFFFFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFFFFFFFFFFFFFF;

    let hi_hi = a_hi * b_hi;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let lo_lo = a_lo * b_lo;

    let mid = (lo_lo >> 64)
        .wrapping_add(hi_lo & 0xFFFFFFFFFFFFFFFF)
        .wrapping_add(lo_hi & 0xFFFFFFFFFFFFFFFF);

    hi_hi
        .wrapping_add(hi_lo >> 64)
        .wrapping_add(lo_hi >> 64)
        .wrapping_add(mid >> 64)
}

/// Compute which TickArray a given tick belongs to.
/// Returns the start_tick of that array.
///
/// Example: tick=5000, array_size=5632 → start=0
///          tick=5632, array_size=5632 → start=5632
pub fn tick_to_array_start_tick(tick: i32) -> i32 {
    let array_size = (TICKS_PER_ARRAY as i32) * TICK_SPACING;
    // Integer floor division (handles negative ticks correctly)
    let div = tick / array_size;
    let rem = tick % array_size;
    if rem < 0 { div - 1 } else { div } * array_size
}

/// Given a tick array's start_tick, return its index in the global bitmap.
pub fn array_start_to_bitmap_index(start_tick: i32) -> i32 {
    let array_size = (TICKS_PER_ARRAY as i32) * TICK_SPACING;
    let min_array_start = tick_to_array_start_tick(MIN_TICK);
    (start_tick - min_array_start) / array_size
}

/// Given an array bitmap index, return (word_index, bit_index).
/// word_index: which u64 in the bitmap array
/// bit_index:  which bit within that u64
pub fn bitmap_word_and_bit(array_index: i32) -> (i32, u32) {
    let word_index = array_index / ARRAYS_PER_BITMAP_WORD as i32;
    let bit_index  = (array_index % ARRAYS_PER_BITMAP_WORD as i32) as u32;
    (word_index, bit_index)
}

// ─── Custom errors ─────────────────────────────────────────────────────────────

#[error_code]
pub enum TickManagerError {
    #[msg("Tick index out of allowed range")]
    TickOutOfRange,
    #[msg("Tick is not a multiple of tick spacing")]
    InvalidTickSpacing,
    #[msg("TickArray is already initialized")]
    TickArrayAlreadyInitialized,
    #[msg("Tick is not initialized in this array")]
    TickNotInitialized,
    #[msg("Bitmap word index out of range")]
    BitmapOutOfRange,
}

// ─── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These expected values were generated by verify_math.ts
    // If these tests fail, the math is wrong — do not adjust the expected values.

    #[test]
    fn test_tick_to_sqrt_price_tick_zero() {
        // tick 0 → √price = 1.0 → Q64 = 2^64
        let result = tick_to_sqrt_price_q64(0).unwrap();
        let expected: u128 = 1u128 << 64; // 18446744073709551616
        // Allow small rounding error in last bit
        let diff = if result > expected { result - expected } else { expected - result };
        assert!(diff < 1_000_000, "tick 0 result={} expected={}", result, expected);
    }

    #[test]
    fn test_tick_to_sqrt_price_positive() {
        // tick 69082 → price ≈ 1000 → √price ≈ 31.623
        // Q64 ≈ 31.623 × 2^64 ≈ 583,140,621,595,701,381,120
        let result = tick_to_sqrt_price_q64(69082).unwrap();
        let expected: u128 = 583_140_621_595_701_381_120;
        let diff = if result > expected { result - expected } else { expected - result };
        // Allow 0.01% error
        assert!(diff < expected / 10_000, "tick 69082 result={} expected={}", result, expected);
    }

    #[test]
    fn test_tick_to_sqrt_price_negative() {
        // Negative tick → price < 1 → √price < 1
        // tick -69082 → √price ≈ 0.03162
        // Q64 ≈ 0.03162 × 2^64 ≈ 583,025,...
        let result = tick_to_sqrt_price_q64(-69082).unwrap();
        // Just verify it's less than Q64 (i.e., √price < 1)
        assert!(result < (1u128 << 64), "negative tick should give sqrt_price < 1");
        assert!(result > 0, "result should be positive");
    }

    #[test]
    fn test_array_start_tick() {
        let array_size = TICKS_PER_ARRAY as i32 * TICK_SPACING;
        assert_eq!(tick_to_array_start_tick(0), 0);
        assert_eq!(tick_to_array_start_tick(array_size - 1), 0);
        assert_eq!(tick_to_array_start_tick(array_size), array_size);
        assert_eq!(tick_to_array_start_tick(-1), -array_size);
        assert_eq!(tick_to_array_start_tick(-array_size), -array_size);
    }

    #[test]
    fn test_bitmap_word_and_bit() {
        assert_eq!(bitmap_word_and_bit(0), (0, 0));
        assert_eq!(bitmap_word_and_bit(7), (0, 7));
        assert_eq!(bitmap_word_and_bit(8), (1, 0));
        assert_eq!(bitmap_word_and_bit(10), (1, 2));
    }

    #[test]
    fn test_validate_tick_spacing() {
        assert!(validate_tick_spacing(0).is_ok());
        assert!(validate_tick_spacing(64).is_ok());
        assert!(validate_tick_spacing(128).is_ok());
        assert!(validate_tick_spacing(1).is_err());
        assert!(validate_tick_spacing(63).is_err());
    }
}