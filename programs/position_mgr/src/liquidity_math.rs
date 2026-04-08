// position_mgr/src/liquidity_math.rs
//
// WHAT THIS FILE DOES:
// Converts between "token amounts + price range" and "liquidity L".
//
// WHY WE NEED THIS:
// The pool works in liquidity units (L), not token amounts.
// When an LP says "I want to deposit 1 SOL and 150 USDC in range $140-$160",
// we need to convert that into one number L that the pool can use.
//
// THE TWO FORMULAS (derive them once, use them always):
//
// Case A: current price is INSIDE the range [lower, upper]
//   Both tokens are needed.
//   L from token0: L = amount0 / (1/sqrt_lower - 1/sqrt_upper)
//                    = amount0 * sqrt_lower * sqrt_upper / (sqrt_upper - sqrt_lower)
//   L from token1: L = amount1 / (sqrt_upper - sqrt_lower)
//   We take the minimum — you can only use as much of each token as the range allows.
//
// Case B: current price is BELOW the range (only token0 needed)
//   L = amount0 * sqrt_lower * sqrt_upper / (sqrt_upper - sqrt_lower)
//
// Case C: current price is ABOVE the range (only token1 needed)
//   L = amount1 / (sqrt_upper - sqrt_lower)
//
// All sqrt prices are Q64.64 format. Liquidity is a raw u128.

// ─── Liquidity for amounts ────────────────────────────────────────────────────

/// Compute liquidity L from token amounts and a price range.
/// Returns L as a u128.
///
/// sqrt_price_current, sqrt_price_lower, sqrt_price_upper: all Q64.64
/// amount_0: token0 amount in raw units (e.g. lamports for SOL)
/// amount_1: token1 amount in raw units (e.g. micro-USDC)
pub fn get_liquidity_for_amounts(
    sqrt_price_current: u128,
    sqrt_price_lower: u128,
    sqrt_price_upper: u128,
    amount_0: u64,
    amount_1: u64,
) -> u128 {
    if sqrt_price_lower >= sqrt_price_upper {
        return 0;
    }

    if sqrt_price_current <= sqrt_price_lower {
        // Current price below range — only token0 active
        get_liquidity_for_amount_0(sqrt_price_lower, sqrt_price_upper, amount_0 as u128)
    } else if sqrt_price_current >= sqrt_price_upper {
        // Current price above range — only token1 active
        get_liquidity_for_amount_1(sqrt_price_lower, sqrt_price_upper, amount_1 as u128)
    } else {
        // Current price inside range — take minimum of both
        let l0 = get_liquidity_for_amount_0(sqrt_price_current, sqrt_price_upper, amount_0 as u128);
        let l1 = get_liquidity_for_amount_1(sqrt_price_lower, sqrt_price_current, amount_1 as u128);
        l0.min(l1)
    }
}

/// L from token0: L = amount0 * sqrt_a * sqrt_b / (sqrt_b - sqrt_a)
/// (amount0 is the SOL-like token; quantity decreases as price rises)
fn get_liquidity_for_amount_0(sqrt_price_a: u128, sqrt_price_b: u128, amount_0: u128) -> u128 {
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    let diff = upper.saturating_sub(lower);
    if diff == 0 {
        return 0;
    }

    // L = amount * lower * upper / (upper - lower)
    // All in Q64. To avoid overflow: (amount * lower / diff) * upper / 2^64
    // We divide step by step to stay in u128 range.
    let intermediate = amount_0
        .saturating_mul(lower >> 32)
        .checked_div(diff >> 32)
        .unwrap_or(0);

    intermediate
        .saturating_mul(upper >> 32)
        .checked_div(1u128 << 32) // divide by 2^32 to correct for the two >> 32 shifts
        .unwrap_or(0)
}

/// L from token1: L = amount1 / (sqrt_b - sqrt_a) * 2^64
/// (amount1 is the USDC-like token; quantity increases as price rises)
fn get_liquidity_for_amount_1(sqrt_price_a: u128, sqrt_price_b: u128, amount_1: u128) -> u128 {
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    let diff = upper.saturating_sub(lower);
    if diff == 0 {
        return 0;
    }

    // L = amount1 * 2^64 / diff  (diff is Q64, so this gives raw L)
    amount_1
        .checked_shl(64)
        .unwrap_or(u128::MAX)
        .checked_div(diff)
        .unwrap_or(0)
}

// ─── Amounts for liquidity ────────────────────────────────────────────────────

/// Compute token amounts to return when removing liquidity L.
/// Inverse of get_liquidity_for_amounts.
///
/// Returns (amount_0, amount_1) in raw token units.
pub fn get_amounts_for_liquidity(
    sqrt_price_current: u128,
    sqrt_price_lower: u128,
    sqrt_price_upper: u128,
    liquidity: u128,
) -> (u64, u64) {
    if sqrt_price_lower >= sqrt_price_upper || liquidity == 0 {
        return (0, 0);
    }

    let amount_0: u128;
    let amount_1: u128;

    if sqrt_price_current <= sqrt_price_lower {
        // Below range: all token0
        amount_0 = get_amount_0_for_liquidity(sqrt_price_lower, sqrt_price_upper, liquidity);
        amount_1 = 0;
    } else if sqrt_price_current >= sqrt_price_upper {
        // Above range: all token1
        amount_0 = 0;
        amount_1 = get_amount_1_for_liquidity(sqrt_price_lower, sqrt_price_upper, liquidity);
    } else {
        // Inside range: split between both tokens
        amount_0 = get_amount_0_for_liquidity(sqrt_price_current, sqrt_price_upper, liquidity);
        amount_1 = get_amount_1_for_liquidity(sqrt_price_lower, sqrt_price_current, liquidity);
    }

    // Clamp to u64 max (overflow would indicate a bug in liquidity calculation)
    (
        amount_0.min(u64::MAX as u128) as u64,
        amount_1.min(u64::MAX as u128) as u64,
    )
}

/// amount0 = L * (sqrt_b - sqrt_a) / (sqrt_a * sqrt_b)
/// token0 amount between two sqrt prices for given liquidity
fn get_amount_0_for_liquidity(sqrt_price_a: u128, sqrt_price_b: u128, liquidity: u128) -> u128 {
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    if lower == 0 {
        return 0;
    }

    let diff = upper.saturating_sub(lower);

    // amount = L * diff / (lower * upper / 2^64)
    //        = L * diff * 2^64 / (lower * upper)
    // Step by step to avoid overflow:
    let numerator = liquidity.saturating_mul(diff);

    // lower * upper >> 64 (both are Q64, product is Q128, shift back to Q64)
    let denom_hi = (lower >> 32).saturating_mul(upper >> 32);
    if denom_hi == 0 {
        return 0;
    }

    numerator.checked_div(denom_hi).unwrap_or(0)
}

/// amount1 = L * (sqrt_b - sqrt_a) / 2^64
/// token1 amount between two sqrt prices for given liquidity
fn get_amount_1_for_liquidity(sqrt_price_a: u128, sqrt_price_b: u128, liquidity: u128) -> u128 {
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    let diff = upper.saturating_sub(lower);

    // amount = L * diff / 2^64
    liquidity.saturating_mul(diff) >> 64
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Q64 scale factor
    const Q64: u128 = 1u128 << 64;

    // sqrt price helpers for tests: price P -> sqrt_P * 2^64
    fn sqrt_q64(price_f64: f64) -> u128 {
        (price_f64.sqrt() * (Q64 as f64)) as u128
    }

    #[test]
    fn test_liquidity_round_trip_inside_range() {
        // LP deposits at price $150, range $140-$160
        let sqrt_current = sqrt_q64(150.0);
        let sqrt_lower = sqrt_q64(140.0);
        let sqrt_upper = sqrt_q64(160.0);

        let amount_0 = 1_000_000u64; // 1 token0
        let amount_1 = 150_000_000u64; // 150 token1

        let liquidity =
            get_liquidity_for_amounts(sqrt_current, sqrt_lower, sqrt_upper, amount_0, amount_1);

        assert!(liquidity > 0, "liquidity should be positive");

        // Round-trip: get amounts back
        let (back_0, back_1) =
            get_amounts_for_liquidity(sqrt_current, sqrt_lower, sqrt_upper, liquidity);

        // Should get back close to what we put in (rounding loss is expected)
        // Accept within 1% for integer math
        let ratio_0 = back_0 as f64 / amount_0 as f64;
        let ratio_1 = back_1 as f64 / amount_1 as f64;

        assert!(
            ratio_0 > 0.95 && ratio_0 <= 1.0,
            "amount0 round-trip ratio={:.4} expected 0.95..1.0",
            ratio_0
        );
        assert!(
            ratio_1 > 0.95 && ratio_1 <= 1.0,
            "amount1 round-trip ratio={:.4} expected 0.95..1.0",
            ratio_1
        );
    }

    #[test]
    fn test_liquidity_below_range_only_token0() {
        // Price below range: only token0 should matter
        let sqrt_current = sqrt_q64(130.0); // below $140
        let sqrt_lower = sqrt_q64(140.0);
        let sqrt_upper = sqrt_q64(160.0);

        let liquidity = get_liquidity_for_amounts(
            sqrt_current,
            sqrt_lower,
            sqrt_upper,
            1_000_000,
            999_999_999, // big token1 amount should be ignored
        );

        let liquidity_token0_only = get_liquidity_for_amounts(
            sqrt_current,
            sqrt_lower,
            sqrt_upper,
            1_000_000,
            0, // zero token1
        );

        // Both should produce the same result (token1 ignored below range)
        assert_eq!(
            liquidity, liquidity_token0_only,
            "below range: token1 should be ignored"
        );
    }

    #[test]
    fn test_liquidity_above_range_only_token1() {
        // Price above range: only token1 should matter
        let sqrt_current = sqrt_q64(170.0); // above $160
        let sqrt_lower = sqrt_q64(140.0);
        let sqrt_upper = sqrt_q64(160.0);

        let liquidity = get_liquidity_for_amounts(
            sqrt_current,
            sqrt_lower,
            sqrt_upper,
            999_999_999,
            150_000_000, // big token0 should be ignored
        );

        let liquidity_token1_only = get_liquidity_for_amounts(
            sqrt_current,
            sqrt_lower,
            sqrt_upper,
            0,
            150_000_000, // zero token0
        );

        assert_eq!(
            liquidity, liquidity_token1_only,
            "above range: token0 should be ignored"
        );
    }

    #[test]
    fn test_zero_amounts_give_zero_liquidity() {
        let sqrt_p = sqrt_q64(150.0);
        let l = get_liquidity_for_amounts(sqrt_p, sqrt_p, sqrt_p, 0, 0);
        assert_eq!(l, 0);
    }
}
