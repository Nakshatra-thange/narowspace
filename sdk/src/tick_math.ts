/**
 * tick_math.ts
 *
 * WHAT THIS FILE IS:
 * The TypeScript mirror of our on-chain tick math.
 * We write this first so we can verify numbers before trusting Rust output.
 * Every function here has a Rust counterpart in tick_manager/src/math.rs
 *
 * CORE IDEA:
 * A "tick" is just an integer index into the price axis.
 * Price at tick i = 1.0001^i
 * Tick 0     → price 1.0
 * Tick 10000 → price ~2.718  (≈ e, coincidence)
 * Tick 69082 → price ~1000.0
 *
 * We store √price (square root of price) not price itself.
 * Why? The swap math cancels out cleanly with √price, avoiding precision loss.
 */

import BN from "bn.js";

// ─── Constants ────────────────────────────────────────────────────────────────

/** Tick spacing: we only use ticks that are multiples of this. */
export const TICK_SPACING = 64;

/** Total ticks in one TickArray account (88 is what Orca uses, we match it). */
export const TICKS_PER_ARRAY = 88;

/** How many TickArrays one bitmap word covers. */
export const ARRAYS_PER_BITMAP_WORD = 8;

/** Min/max tick indices that are valid. */
export const MIN_TICK = -443636;
export const MAX_TICK = 443636;

/**
 * Q64.64 fixed-point scale factor.
 * We store √price as a Q64.64 number: integer part in top 64 bits, fraction in bottom 64.
 * This gives us 64 bits of precision without floating point on-chain.
 */
export const Q64 = new BN(1).shln(64); // 2^64

// ─── Tick ↔ Price conversions ─────────────────────────────────────────────────

/**
 * tickToPrice
 * Formula: price = 1.0001^tick
 * Returns a regular JS number (float). Use only off-chain for display.
 *
 * Example:
 *   tickToPrice(0)     → 1.0
 *   tickToPrice(10000) → 2.7181...
 *   tickToPrice(69082) → ~1000
 */
export function tickToPrice(tick: number): number {
  return Math.pow(1.0001, tick);
}

/**
 * tickToSqrtPrice
 * Returns √(1.0001^tick) as a JS float.
 * This is what the pool actually stores and uses in swap math.
 */
export function tickToSqrtPrice(tick: number): number {
  return Math.sqrt(tickToPrice(tick));
}

/**
 * priceToTick
 * Inverse of tickToPrice.
 * Formula: tick = log(price) / log(1.0001)
 * Rounds DOWN so the tick is always ≤ the actual price.
 *
 * Example:
 *   priceToTick(150)  → tick for $150 SOL
 *   priceToTick(1000) → 69082
 */
export function priceToTick(price: number): number {
  if (price <= 0) throw new Error("Price must be positive");
  const tick = Math.log(price) / Math.log(1.0001);
  return Math.floor(tick);
}

/**
 * nearestUsableTick
 * Rounds a raw tick down to the nearest multiple of TICK_SPACING.
 * We only initialize ticks at multiples of tick spacing — this keeps
 * the bitmap compact and the swap loop fast.
 *
 * Example with TICK_SPACING=64:
 *   nearestUsableTick(100)  → 64
 *   nearestUsableTick(128)  → 128
 *   nearestUsableTick(-10)  → -64
 */
export function nearestUsableTick(tick: number): number {
  const spacing = TICK_SPACING;
  const rounded = Math.floor(tick / spacing) * spacing;
  if (rounded < MIN_TICK) return MIN_TICK + TICK_SPACING - (MIN_TICK % TICK_SPACING);
  if (rounded > MAX_TICK) return MAX_TICK - (MAX_TICK % TICK_SPACING);
  return rounded;
}

// ─── Q64.64 fixed-point helpers ───────────────────────────────────────────────

/**
 * sqrtPriceToQ64
 * Converts a float √price into our Q64.64 on-chain representation.
 * Multiply by 2^64, take the integer part.
 *
 * We use BN (big number) because JS floats lose precision at 2^64 scale.
 */
export function sqrtPriceToQ64(sqrtPrice: number): BN {
  // sqrtPrice * 2^64, stored as integer
  // We use string conversion via BigInt to avoid float precision loss
  const scaled = BigInt(Math.floor(sqrtPrice * 2 ** 32)) * BigInt(2 ** 32);
  return new BN(scaled.toString());
}

/**
 * q64ToSqrtPrice
 * Inverse: convert Q64.64 back to float for display purposes.
 */
export function q64ToSqrtPrice(q64: BN): number {
  const Q64 = new BN(1).shln(64);

  const integerPart = q64.div(Q64).toNumber(); // safe (small)
  const fractionalPartBN = q64.mod(Q64);

  const fractionalPart =
    Number(fractionalPartBN.toString()) / 2 ** 64;

  return integerPart + fractionalPart;
}

/**
 * tickToSqrtPriceQ64
 * The main function the pool uses: give me a tick, I give you √price in Q64.64.
 * This is what gets stored in Pool.sqrt_price.
 */
export function tickToSqrtPriceQ64(tick: number): BN {
  return sqrtPriceToQ64(tickToSqrtPrice(tick));
}

// ─── Bitmap helpers ───────────────────────────────────────────────────────────

/**
 * BITMAP STRUCTURE — read this carefully:
 *
 * We have millions of possible ticks. We can't store one account per tick.
 * Solution: two-level structure.
 *
 * Level 1 — TickArray:
 *   One account holds 88 consecutive ticks.
 *   TickArray for "start tick 0" holds ticks 0, 64, 128, ... 5568 (88 × 64)
 *
 * Level 2 — Bitmap:
 *   One bitmap word (a u64) tracks 8 TickArrays.
 *   Bit i is ON → TickArray i has at least one initialized tick.
 *   This lets the swap engine skip empty regions instantly.
 *
 * Lookup to find next active tick:
 *   1. Check bitmap word → find next ON bit → that's the TickArray index
 *   2. Load that TickArray account → scan 88 ticks → find initialized one
 */

/**
 * tickToArrayStartTick
 * Given any tick, find the start tick of its TickArray.
 * TickArray boundaries are at multiples of (TICKS_PER_ARRAY × TICK_SPACING).
 *
 * Example: tick 5000, TICK_SPACING=64, TICKS_PER_ARRAY=88
 *   array size = 88 × 64 = 5632 ticks wide
 *   5000 / 5632 = 0  → start tick = 0
 *   tick 6000 / 5632 = 1 → start tick = 5632
 */
export function tickToArrayStartTick(tick: number): number {
  const arraySize = TICKS_PER_ARRAY * TICK_SPACING;
  return Math.floor(tick / arraySize) * arraySize;
}

/**
 * arrayStartTickToBitmapIndex
 * Maps a TickArray's start tick to its index in the bitmap.
 * Index 0 is the array starting at MIN_TICK.
 */
export function arrayStartTickToBitmapIndex(startTick: number): number {
  const arraySize = TICKS_PER_ARRAY * TICK_SPACING;
  const minArrayStart = tickToArrayStartTick(MIN_TICK);
  return Math.floor((startTick - minArrayStart) / arraySize);
}

/**
 * bitmapWordAndBit
 * Given an array index, find which bitmap word it lives in and which bit.
 * Returns { wordIndex, bitIndex }.
 *
 * Example: array index 10
 *   wordIndex = 10 / 8 = 1  (second word)
 *   bitIndex  = 10 % 8 = 2  (third bit)
 */
export function bitmapWordAndBit(arrayIndex: number): { wordIndex: number; bitIndex: number } {
  return {
    wordIndex: Math.floor(arrayIndex / ARRAYS_PER_BITMAP_WORD),
    bitIndex: arrayIndex % ARRAYS_PER_BITMAP_WORD,
  };
}

// ─── Liquidity math ───────────────────────────────────────────────────────────

/**
 * getLiquidityForAmounts
 * Given token amounts and a price range, compute the liquidity L.
 *
 * WHY THIS MATTERS:
 * When an LP says "I'm depositing 100 USDC and 1 SOL between $140–$160",
 * we need to convert that into a single number L (liquidity units).
 * L is what the swap engine uses — not the raw token amounts.
 *
 * SIMPLIFIED FORMULA:
 * For a range [tickLower, tickUpper] with current tick in range:
 *   L = amount0 / (1/√priceLower - 1/√priceUpper)   [for token0, e.g. SOL]
 *   L = amount1 / (√priceUpper - √priceLower)         [for token1, e.g. USDC]
 * We take the minimum — you can't use more of one token than the range allows.
 *
 * For now we return a BN representing liquidity units.
 */
export function getLiquidityForAmounts(
  sqrtPriceCurrent: number,
  sqrtPriceLower: number,
  sqrtPriceUpper: number,
  amount0: number, // token0 amount (e.g. SOL)
  amount1: number  // token1 amount (e.g. USDC)
): BN {
  let liquidity: number;

  if (sqrtPriceCurrent <= sqrtPriceLower) {
    // Current price is below range — only token0 is relevant
    liquidity = amount0 / (1 / sqrtPriceLower - 1 / sqrtPriceUpper);
  } else if (sqrtPriceCurrent >= sqrtPriceUpper) {
    // Current price is above range — only token1 is relevant
    liquidity = amount1 / (sqrtPriceUpper - sqrtPriceLower);
  } else {
    // Current price is inside range — both tokens contribute
    const l0 = amount0 / (1 / sqrtPriceCurrent - 1 / sqrtPriceUpper);
    const l1 = amount1 / (sqrtPriceCurrent - sqrtPriceLower);
    liquidity = Math.min(l0, l1);
  }

  return new BN(Math.floor(liquidity));
}

/**
 * getAmountsForLiquidity
 * Inverse: given L and a price range, how many tokens does the LP get back?
 * Used when removing liquidity.
 */
export function getAmountsForLiquidity(
  sqrtPriceCurrent: number,
  sqrtPriceLower: number,
  sqrtPriceUpper: number,
  liquidity: BN
): { amount0: BN; amount1: BN } {
  const L = Number(liquidity.toString());;
  let amount0 = 0;
  let amount1 = 0;

  if (sqrtPriceCurrent <= sqrtPriceLower) {
    amount0 = L * (1 / sqrtPriceLower - 1 / sqrtPriceUpper);
  } else if (sqrtPriceCurrent >= sqrtPriceUpper) {
    amount1 = L * (sqrtPriceUpper - sqrtPriceLower);
  } else {
    amount0 = L * (1 / sqrtPriceCurrent - 1 / sqrtPriceUpper);
    amount1 = L * (sqrtPriceCurrent - sqrtPriceLower);
  }

  return {
    amount0: new BN(Math.floor(amount0)),
    amount1: new BN(Math.floor(amount1)),
  };
}