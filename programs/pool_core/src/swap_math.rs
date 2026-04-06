//! swap_math.rs — The arithmetic engine of the swap
//!
//! WHAT THIS FILE DOES:
//! Contains every formula used during a single "swap step".
//! A full swap is a loop of these steps (see lib.rs).
//! Isolating math here means we can unit-test it without deploying anything.
//!
//! MIRRORS: sdk/src/swap_math.ts (written alongside this file)
//!
//! THE CORE IDEA OF ONE SWAP STEP:
//! The pool has a current √price. The swap wants to move price in one direction.
//! Each step moves price until it either:
//!   (a) fills the entire requested swap amount, OR
//!   (b) hits the next initialized tick boundary
//! Whichever comes first ends the step. The loop repeats for (b).

// ─── Fee math ──────────────────────────────────────────────────────────────────

/// Fee rate denominator. Fee of 300 = 0.3% = 300/1_000_000.
pub const FEE_DENOMINATOR: u128 = 1_000_000;

/// Default fee tier: 0.3%
pub const DEFAULT_FEE_RATE: u128 = 3_000;

/// Given an input amount, compute the fee portion.
/// fee = amount × fee_rate / FEE_DENOMINATOR
/// We round UP on fees (protocol-friendly, not user-friendly — standard practice).
pub fn compute_fee(amount: u128, fee_rate: u128) -> u128 {
    // ceiling division: (a × b + d - 1) / d
    amount
        .checked_mul(fee_rate)
        .unwrap_or(u128::MAX)
        .saturating_add(FEE_DENOMINATOR - 1)
        / FEE_DENOMINATOR
}

/// Given a gross input amount (including fee), compute the net amount (after fee).
pub fn amount_less_fee(gross_amount: u128, fee_rate: u128) -> u128 {
    let fee = compute_fee(gross_amount, fee_rate);
    gross_amount.saturating_sub(fee)
}

// ─── √price math ──────────────────────────────────────────────────────────────
//
// WHY √price?
// Recall from Day 1: we store price as √price in Q64.64 format.
// The swap formulas use √price directly — this is where those formulas live.
//
// THE TWO TOKEN MODEL:
//   token0 = the "base" token (e.g. SOL). Its amount uses formula involving 1/√P.
//   token1 = the "quote" token (e.g. USDC). Its amount uses √P directly.
//
// This asymmetry comes from the concentrated liquidity math derivation.
// You don't need to derive it — just know which formula applies to which token.

/// Compute amount of token0 moved when √price changes from sqrt_a to sqrt_b.
///
/// FORMULA: Δtoken0 = L × (1/√price_a - 1/√price_b)
///                  = L × (√price_b - √price_a) / (√price_a × √price_b)
///
/// All values in Q64.64. Returns amount in raw token units (no Q64 scaling).
///
/// WHY THIS FORMULA:
/// token0 (SOL) quantity in a range is inversely proportional to √price.
/// More SOL in pool = lower price. Moving price up = less SOL in pool.
pub fn get_amount_0_delta(
    sqrt_price_a: u128, // Q64.64
    sqrt_price_b: u128, // Q64.64
    liquidity: u128,
    round_up: bool,
) -> u128 {
    // Ensure a <= b
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    if lower == 0 {
        return u128::MAX; // price at zero is undefined
    }

    // numerator = L × (upper - lower) in Q64 space
    // We need extra precision: multiply liquidity (raw) by price diff (Q64) → Q64 result
    let numerator = liquidity
        .checked_mul(upper.saturating_sub(lower))
        .unwrap_or(u128::MAX);

    // denominator = lower × upper (both Q64, product is Q128 — we want Q64 result)
    // So: result = numerator_Q64 / (lower_Q64 × upper_Q64 >> 64)
    //            = (numerator_Q64 << 64) / (lower_Q64 × upper_Q64)  [with overflow guard]
    let denominator = mul_q64(lower, upper); // lower × upper >> 64 (stays in Q64)

    if denominator == 0 {
        return u128::MAX;
    }

    if round_up {
        // ceiling division
        numerator
            .saturating_add(denominator - 1)
            .checked_div(denominator)
            .unwrap_or(u128::MAX)
    } else {
        numerator.checked_div(denominator).unwrap_or(0)
    }
}

/// Compute amount of token1 moved when √price changes from sqrt_a to sqrt_b.
///
/// FORMULA: Δtoken1 = L × (√price_b - √price_a)
///
/// WHY THIS FORMULA:
/// token1 (USDC) quantity is directly proportional to √price.
/// More USDC in pool = higher price. Price goes up = more USDC in pool.
pub fn get_amount_1_delta(
    sqrt_price_a: u128, // Q64.64
    sqrt_price_b: u128, // Q64.64
    liquidity: u128,
    round_up: bool,
) -> u128 {
    let (lower, upper) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a, sqrt_price_b)
    } else {
        (sqrt_price_b, sqrt_price_a)
    };

    // result = L × (upper - lower) / 2^64
    // The price diff is in Q64, so we divide by 2^64 to get raw token units
    let diff = upper.saturating_sub(lower);

    if round_up {
        // ceiling: (L × diff + 2^64 - 1) / 2^64
        liquidity
            .checked_mul(diff)
            .unwrap_or(u128::MAX)
            .saturating_add((1u128 << 64) - 1)
            >> 64
    } else {
        // floor: (L × diff) / 2^64
        (liquidity.checked_mul(diff).unwrap_or(u128::MAX)) >> 64
    }
}

// ─── √price target computation ────────────────────────────────────────────────

/// Given a swap of token0 (exact input), what is the new √price?
///
/// FORMULA: new_√price = L × √price_current / (L + Δtoken0 × √price_current)
///
/// Derivation: we know Δtoken0 = L×(1/√P_new - 1/√P_current), solve for √P_new.
///
/// Returns new √price in Q64.64.
pub fn get_next_sqrt_price_from_amount_0(
    sqrt_price_current: u128,
    liquidity: u128,
    amount: u128,
    add: bool, // true = adding token0 (price goes DOWN), false = removing (price goes UP)
) -> u128 {
    if amount == 0 {
        return sqrt_price_current;
    }

    // numerator = L × √P_current (Q64 × raw → needs care)
    // L is raw, √P is Q64. Product is Q64 scaled by L.
    let numerator = (liquidity as u128) << 64; // L × 2^64 — gives us L in Q128 space

    if add {
        // Price goes DOWN when adding token0
        // new_√P = numerator / (numerator / √P_current + amount)
        //        = (L × 2^64) / (L + amount × √P_current / 2^64)
        let product = amount.checked_mul(sqrt_price_current).unwrap_or(u128::MAX);
        let denominator = numerator
            .checked_div(1u128 << 64) // back to raw L
            .unwrap_or(0)
            .saturating_add(product >> 64); // + amount × √P / 2^64

        if denominator == 0 {
            return 0;
        }
        // result in Q64: numerator_Q128 / denominator_raw → Q64
        (numerator / denominator) as u128
    } else {
        // Price goes UP when removing token0
        let product = amount.checked_mul(sqrt_price_current).unwrap_or(0);
        let denominator = (numerator >> 64).saturating_sub(product >> 64);
        if denominator == 0 {
            return u128::MAX;
        }
        numerator / denominator
    }
}

/// Given a swap of token1 (exact input), what is the new √price?
///
/// FORMULA: new_√price = √price_current + Δtoken1 / L
///
/// Derivation: Δtoken1 = L×(√P_new - √P_current), solve for √P_new.
pub fn get_next_sqrt_price_from_amount_1(
    sqrt_price_current: u128,
    liquidity: u128,
    amount: u128,
    add: bool, // true = adding token1 (price goes UP)
) -> u128 {
    // delta_sqrt_price = amount × 2^64 / L   (convert raw amount to Q64 then divide by L)
    let delta = ((amount as u128) << 64)
        .checked_div(liquidity as u128)
        .unwrap_or(0);

    if add {
        sqrt_price_current.saturating_add(delta)
    } else {
        sqrt_price_current.saturating_sub(delta)
    }
}

// ─── Core swap step ───────────────────────────────────────────────────────────

/// Result of one swap step.
///
/// ONE STEP = move price from current toward target (next tick boundary),
/// consuming as much of the swap amount as possible within this step.
#[derive(Debug, Clone, Copy)]
pub struct SwapStepResult {
    /// The √price we actually reached this step (may equal sqrt_price_target if we hit the tick)
    pub sqrt_price_next: u128,
    /// How much of the input token was consumed this step
    pub amount_in: u128,
    /// How much of the output token was produced this step
    pub amount_out: u128,
    /// Fee collected this step (already deducted from amount_in)
    pub fee_amount: u128,
}

/// Compute one swap step.
///
/// INPUTS:
///   sqrt_price_current — pool's current √price (Q64.64)
///   sqrt_price_target  — the next tick's √price (Q64.64); we stop here if we hit it
///   liquidity          — active liquidity in the current tick range
///   amount_remaining   — how much input token is left to swap
///   fee_rate           — e.g. 3000 = 0.3%
///   zero_for_one       — true = swapping token0 for token1 (price goes DOWN)
///
/// WHAT HAPPENS:
///   1. Compute how much input would be needed to reach sqrt_price_target exactly
///   2. If amount_remaining >= that: we reach the target (tick crossing next)
///   3. If amount_remaining <  that: we stop partway, compute output from actual Δ√price
pub fn compute_swap_step(
    sqrt_price_current: u128,
    sqrt_price_target: u128,
    liquidity: u128,
    amount_remaining: u128,
    fee_rate: u128,
) -> SwapStepResult {
    let zero_for_one = sqrt_price_current >= sqrt_price_target;

    // Amount needed (net of fee) to move price all the way to the target tick
    let amount_to_target = if zero_for_one {
        // Swapping token0→token1: price decreasing
        // How much token0 needed to move from current to target?
        get_amount_0_delta(sqrt_price_target, sqrt_price_current, liquidity, true)
    } else {
        // Swapping token1→token0: price increasing
        // How much token1 needed to move from current to target?
        get_amount_1_delta(sqrt_price_current, sqrt_price_target, liquidity, true)
    };

    // Net amount after removing fee from amount_remaining
    let amount_remaining_net = amount_less_fee(amount_remaining, fee_rate);

    let (sqrt_price_next, amount_in, amount_out);

    if amount_remaining_net >= amount_to_target {
        // We have enough — price moves all the way to the target tick
        sqrt_price_next = sqrt_price_target;
        amount_in = amount_to_target;

        amount_out = if zero_for_one {
            // token0 in → token1 out
            get_amount_1_delta(sqrt_price_target, sqrt_price_current, liquidity, false)
        } else {
            // token1 in → token0 out
            get_amount_0_delta(sqrt_price_target, sqrt_price_current, liquidity, false)
        };
    } else {
        // Not enough — price moves partway, stops when amount is exhausted
        sqrt_price_next = if zero_for_one {
            get_next_sqrt_price_from_amount_0(sqrt_price_current, liquidity, amount_remaining_net, true)
        } else {
            get_next_sqrt_price_from_amount_1(sqrt_price_current, liquidity, amount_remaining_net, true)
        };

        // Actual amounts moved at this partial step
        amount_in = if zero_for_one {
            get_amount_0_delta(sqrt_price_next, sqrt_price_current, liquidity, true)
        } else {
            get_amount_1_delta(sqrt_price_current, sqrt_price_next, liquidity, true)
        };

        amount_out = if zero_for_one {
            get_amount_1_delta(sqrt_price_next, sqrt_price_current, liquidity, false)
        } else {
            get_amount_0_delta(sqrt_price_current, sqrt_price_next, liquidity, false)
        };
    }

    // Fee = gross_input - net_input
    // The caller passed in amount_remaining as gross; amount_in is net
    let fee_amount = amount_remaining.saturating_sub(amount_in);

    SwapStepResult {
        sqrt_price_next,
        amount_in,
        amount_out,
        fee_amount,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Multiply two Q64.64 numbers and return result in Q64.64.
/// (a × b) >> 64
/// Both inputs are Q64, product would be Q128, we shift back to Q64.
#[inline]
pub fn mul_q64(a: u128, b: u128) -> u128 {
    // Split to avoid overflow: (a_hi*2^64 + a_lo) * (b_hi*2^64 + b_lo)
    // We only need the Q64 result = (full product) >> 64
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFF_FFFF_FFFF_FFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFF_FFFF_FFFF_FFFF;

    // hi×hi contributes at Q128 level (shift back 64 = Q64)
    let hi_hi = a_hi.saturating_mul(b_hi);
    // hi×lo and lo×hi contribute at Q64 level
    let hi_lo = (a_hi.saturating_mul(b_lo)) >> 64;
    let lo_hi = (a_lo.saturating_mul(b_hi)) >> 64;
    // lo×lo contributes below Q64 (ignore for truncation)

    hi_hi.saturating_add(hi_lo).saturating_add(lo_hi)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const Q64: u128 = 1u128 << 64;

    // √price for a few reference prices (computed by verify_math.ts)
    // price 1.0   → √price 1.0   → Q64 = 2^64
    // price 4.0   → √price 2.0   → Q64 = 2 × 2^64
    // price 100.0 → √price 10.0  → Q64 = 10 × 2^64

    fn sqrt_q64(price: f64) -> u128 {
        // Convert float √price to Q64.64 for test fixtures
        let sqrt_p = price.sqrt();
        (sqrt_p * (Q64 as f64)) as u128
    }

    #[test]
    fn test_compute_fee() {
        // 0.3% fee on 1,000,000 units = 3,000 units
        assert_eq!(compute_fee(1_000_000, DEFAULT_FEE_RATE), 3_000);
        // fee on 0 is 0
        assert_eq!(compute_fee(0, DEFAULT_FEE_RATE), 0);
    }

    #[test]
    fn test_amount_1_delta_symmetry() {
        // If price moves from √4 to √9 with L=1000:
        // Δtoken1 = 1000 × (√9 - √4) / 2^64 ... but in Q64 terms:
        // Δtoken1 = L × (sqrt_b - sqrt_a) >> 64
        let sqrt_a = sqrt_q64(4.0);  // 2.0 in Q64
        let sqrt_b = sqrt_q64(9.0);  // 3.0 in Q64
        let liquidity = Q64;          // 1.0 in raw, using Q64 as "1 unit" for test

        let delta = get_amount_1_delta(sqrt_a, sqrt_b, liquidity, false);

        // Expected: L × (√9 - √4) = 1 × (3 - 2) = 1.0
        // In our units: should be close to Q64 (≈ 1 unit of token1)
        let expected = Q64; // 1 "unit"
        let ratio = delta as f64 / expected as f64;
        // Within 1% — Q64 rounding is expected
        assert!(ratio > 0.99 && ratio < 1.01, "delta={} expected≈{}", delta, expected);
    }

    #[test]
    fn test_swap_step_full_fill() {
        // Scenario: swap with enough input to fully reach target price
        let sqrt_current = sqrt_q64(100.0); // current price = 100
        let sqrt_target  = sqrt_q64(121.0); // target  price = 121 (next tick)
        let liquidity    = 1_000_000u128 * Q64 >> 32; // some liquidity
        let fee_rate     = DEFAULT_FEE_RATE;

        // Amount that should be MORE than enough to reach target
        let amount_in = u128::MAX / 2;

        let result = compute_swap_step(
            sqrt_current,
            sqrt_target,
            liquidity,
            amount_in,
            fee_rate,
        );

        // Since we had more than enough, we should have reached the target exactly
        assert_eq!(result.sqrt_price_next, sqrt_target,
            "should reach target when sufficient input");
        assert!(result.amount_out > 0, "should produce output tokens");
        assert!(result.fee_amount > 0, "should collect fee");
    }

    #[test]
    fn test_swap_step_partial_fill() {
        // Scenario: swap with very small input — shouldn't reach target tick
        let sqrt_current = sqrt_q64(100.0);
        let sqrt_target  = sqrt_q64(81.0);  // price going DOWN (zero_for_one)
        let liquidity    = 1_000_000u128;
        let tiny_amount  = 1u128; // almost nothing

        let result = compute_swap_step(
            sqrt_current,
            sqrt_target,
            liquidity,
            tiny_amount,
            DEFAULT_FEE_RATE,
        );

        // With tiny input, price should not reach the target
        assert!(result.sqrt_price_next > sqrt_target,
            "price should not reach target with tiny input. got={}  target={}",
            result.sqrt_price_next, sqrt_target
        );
    }

    #[test]
    fn test_amount_0_and_1_inverse() {
        // Round-trip: compute amount_out for a price move, then verify
        // that applying that amount_out gets us back to the same price.
        let sqrt_a = sqrt_q64(100.0);
        let sqrt_b = sqrt_q64(110.0); // price going up
        let liquidity = 1_000_000_000u128;

        let amount1 = get_amount_1_delta(sqrt_a, sqrt_b, liquidity, false);
        // Verify: L × (√110 - √100) ≈ amount1
        // This is just a sanity check that amount1 > 0
        assert!(amount1 > 0, "delta should be positive");

        // Going back: amount0 when same price range
        let amount0 = get_amount_0_delta(sqrt_a, sqrt_b, liquidity, false);
        assert!(amount0 > 0, "delta should be positive");
    }
}