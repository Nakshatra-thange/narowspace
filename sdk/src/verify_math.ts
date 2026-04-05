/**
 * verify_math.ts
 *
 * RUN THIS BEFORE WRITING RUST.
 * Prints expected values for every math function.
 * When Rust gives different answers, this is your ground truth.
 *
 * Run: npx ts-node sdk/src/verify_math.ts
 */

import {
    tickToPrice,
    tickToSqrtPrice,
    priceToTick,
    nearestUsableTick,
    tickToSqrtPriceQ64,
    q64ToSqrtPrice,
    tickToArrayStartTick,
    arrayStartTickToBitmapIndex,
    bitmapWordAndBit,
    getLiquidityForAmounts,
    getAmountsForLiquidity,
    TICK_SPACING,
    TICKS_PER_ARRAY,
  } from "./tick_math.ts";
  
  // ─── Separator helper ──────────────────────────────────────────────────────────
  function section(title: string) {
    console.log("\n" + "─".repeat(50));
    console.log(`  ${title}`);
    console.log("─".repeat(50));
  }
  
  // ─── 1. Tick ↔ Price ──────────────────────────────────────────────────────────
  section("Tick → Price (1.0001^tick)");
  const testTicks = [0, 1000, 10000, 27000, 69082, -1000, -10000];
  for (const tick of testTicks) {
    const price = tickToPrice(tick);
    const sqrtP = tickToSqrtPrice(tick);
    console.log(`  tick ${String(tick).padStart(7)} → price ${price.toFixed(6).padStart(14)}  √price ${sqrtP.toFixed(6)}`);
  }
  
  // ─── 2. Price → Tick ──────────────────────────────────────────────────────────
  section("Price → Tick  (log(price) / log(1.0001))");
  const testPrices = [1, 2, 10, 100, 150, 200, 1000];
  for (const price of testPrices) {
    const tick = priceToTick(price);
    const roundTrip = tickToPrice(tick);
    console.log(`  price ${String(price).padStart(6)} → tick ${String(tick).padStart(7)}  round-trip price ${roundTrip.toFixed(4)}`);
  }
  
  // ─── 3. Nearest usable tick ───────────────────────────────────────────────────
  section(`Nearest usable tick (TICK_SPACING=${TICK_SPACING})`);
  const rawTicks = [0, 1, 63, 64, 65, 127, 128, -1, -64, -65, 500, -500];
  for (const raw of rawTicks) {
    const usable = nearestUsableTick(raw);
    console.log(`  raw tick ${String(raw).padStart(5)} → usable tick ${String(usable).padStart(6)}`);
  }
  
  // ─── 4. Q64.64 representation ─────────────────────────────────────────────────
  section("Tick → √price Q64.64 (on-chain representation)");
  const q64Ticks = [0, 27000, 69082, -10000];
  for (const tick of q64Ticks) {
    const q64 = tickToSqrtPriceQ64(tick);
    const back = q64ToSqrtPrice(q64);
    const expected = tickToSqrtPrice(tick);
    const error = Math.abs(back - expected) / expected;
    console.log(`  tick ${String(tick).padStart(7)} → Q64=${q64.toString().padStart(25)}  decoded=${back.toFixed(6)}  expected=${expected.toFixed(6)}  err=${(error * 100).toExponential(2)}%`);
  }
  
  // ─── 5. TickArray start ticks ─────────────────────────────────────────────────
  section(`TickArray start ticks (array size = ${TICKS_PER_ARRAY} × ${TICK_SPACING} = ${TICKS_PER_ARRAY * TICK_SPACING} ticks)`);
  const arrayTestTicks = [0, 100, 5631, 5632, 5633, 11264, -1, -5632, -5633];
  for (const tick of arrayTestTicks) {
    const start = tickToArrayStartTick(tick);
    console.log(`  tick ${String(tick).padStart(7)} → array start ${String(start).padStart(7)}`);
  }
  
  // ─── 6. Bitmap index ──────────────────────────────────────────────────────────
  section("Array start tick → bitmap word + bit");
  const startTicks = [0, 5632, 11264, 16896, 22528];
  for (const start of startTicks) {
    const idx = arrayStartTickToBitmapIndex(start);
    const { wordIndex, bitIndex } = bitmapWordAndBit(idx);
    console.log(`  start=${String(start).padStart(7)} → arrayIdx=${String(idx).padStart(4)}  word=${wordIndex}  bit=${bitIndex}`);
  }
  
  // ─── 7. Liquidity calculation ─────────────────────────────────────────────────
  section("Liquidity for amounts (SOL/USDC pool, SOL ≈ $150)");
  // Scenario: LP puts 1 SOL + 150 USDC into range $140–$160
  const priceCurrent = 150;
  const priceLower   = 140;
  const priceUpper   = 160;
  const sqrtCurrent  = Math.sqrt(priceCurrent);
  const sqrtLower    = Math.sqrt(priceLower);
  const sqrtUpper    = Math.sqrt(priceUpper);
  const amount0      = 1;   // 1 SOL
  const amount1      = 150; // 150 USDC
  
  const L = getLiquidityForAmounts(sqrtCurrent, sqrtLower, sqrtUpper, amount0, amount1);
  console.log(`  Input:  ${amount0} SOL + ${amount1} USDC  |  range $${priceLower}–$${priceUpper}  |  current $${priceCurrent}`);
  console.log(`  Liquidity L = ${L.toString()}`);
  
  // Round-trip: get amounts back from L
  const { amount0: back0, amount1: back1 } = getAmountsForLiquidity(sqrtCurrent, sqrtLower, sqrtUpper, L);
  console.log(`  Round-trip: ${back0.toString()} SOL units  +  ${back1.toString()} USDC units`);
  console.log(`  (small rounding loss is expected — we floor to integers)`);
  
  console.log("\n✅ Math verification complete. These are your ground-truth values.\n");