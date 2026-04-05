/**
 * swap_math.ts
 * TypeScript mirror of pool_core/src/swap_math.rs
 *
 * Uses BN throughout to match Anchor's type system.
 * BN is what program.methods receives — no bigint/BN mismatch.
 */

 import BN from "bn.js";

 // ─── Constants ────────────────────────────────────────────────────────────────
 
 export const FEE_DENOMINATOR = new BN(1_000_000);
 export const DEFAULT_FEE_RATE = new BN(3_000); // 0.3%
 export const MAX_TICK_CROSSINGS = 10;
 
 const Q64 = new BN(1).shln(64); // 2^64
 
 // ─── Fee helpers ──────────────────────────────────────────────────────────────
 
 export function computeFee(amount: BN, feeRate: BN): BN {
   // ceiling division: (amount * feeRate + FEE_DENOMINATOR - 1) / FEE_DENOMINATOR
   return amount
     .mul(feeRate)
     .add(FEE_DENOMINATOR.subn(1))
     .div(FEE_DENOMINATOR);
 }
 
 export function amountLessFee(grossAmount: BN, feeRate: BN): BN {
   return grossAmount.sub(computeFee(grossAmount, feeRate));
 }
 
 // ─── Q64.64 multiply ──────────────────────────────────────────────────────────
 
 function mulQ64(a: BN, b: BN): BN {
   return a.mul(b).shrn(64);
 }
 
 // ─── Amount delta formulas ────────────────────────────────────────────────────
 
 /**
  * Δtoken0 = L * (sqrtB - sqrtA) / (sqrtA * sqrtB)
  * All values Q64.64. Returns raw token units.
  */
 export function getAmount0Delta(
   sqrtPriceA: BN,
   sqrtPriceB: BN,
   liquidity: BN,
   roundUp: boolean
 ): BN {
   const [lower, upper] = sqrtPriceA.lte(sqrtPriceB)
     ? [sqrtPriceA, sqrtPriceB]
     : [sqrtPriceB, sqrtPriceA];
 
   if (lower.isZero()) return new BN(Number.MAX_SAFE_INTEGER);
 
   const numerator   = liquidity.mul(upper.sub(lower));
   const denominator = mulQ64(lower, upper);
 
   if (denominator.isZero()) return new BN(0);
 
   if (roundUp) {
     return numerator.add(denominator.subn(1)).div(denominator);
   }
   return numerator.div(denominator);
 }
 
 /**
  * Δtoken1 = L * (sqrtB - sqrtA)
  * All values Q64.64. Returns raw token units.
  */
 export function getAmount1Delta(
   sqrtPriceA: BN,
   sqrtPriceB: BN,
   liquidity: BN,
   roundUp: boolean
 ): BN {
   const [lower, upper] = sqrtPriceA.lte(sqrtPriceB)
     ? [sqrtPriceA, sqrtPriceB]
     : [sqrtPriceB, sqrtPriceA];
 
   const diff = upper.sub(lower);
 
   if (roundUp) {
     return liquidity.mul(diff).add(Q64.subn(1)).shrn(64);
   }
   return liquidity.mul(diff).shrn(64);
 }
 
 // ─── Next sqrt price computation ──────────────────────────────────────────────
 
 export function getNextSqrtPriceFromAmount0(
   sqrtPriceCurrent: BN,
   liquidity: BN,
   amount: BN,
   add: boolean
 ): BN {
   if (amount.isZero()) return sqrtPriceCurrent;
 
   const numerator = liquidity.shln(64); // L * 2^64
 
   if (add) {
     const product     = amount.mul(sqrtPriceCurrent);
     const denominator = numerator.shrn(64).add(product.shrn(64));
     if (denominator.isZero()) return new BN(0);
     return numerator.div(denominator);
   } else {
     const product     = amount.mul(sqrtPriceCurrent);
     const denominator = numerator.shrn(64).sub(product.shrn(64));
     if (denominator.isZero()) return new BN("ffffffffffffffffffffffffffffffff", 16);
     return numerator.div(denominator);
   }
 }
 
 export function getNextSqrtPriceFromAmount1(
   sqrtPriceCurrent: BN,
   liquidity: BN,
   amount: BN,
   add: boolean
 ): BN {
   const delta = amount.shln(64).div(liquidity);
   return add
     ? sqrtPriceCurrent.add(delta)
     : sqrtPriceCurrent.sub(delta);
 }
 
 // ─── Swap step ────────────────────────────────────────────────────────────────
 
 export interface SwapStepResult {
   sqrtPriceNext: BN;
   amountIn:      BN;
   amountOut:     BN;
   feeAmount:     BN;
 }
 
 export function computeSwapStep(
   sqrtPriceCurrent: BN,
   sqrtPriceTarget:  BN,
   liquidity:        BN,
   amountRemaining:  BN,
   feeRate:          BN
 ): SwapStepResult {
   const zeroForOne = sqrtPriceCurrent.gte(sqrtPriceTarget);
 
   const amountToTarget = zeroForOne
     ? getAmount0Delta(sqrtPriceTarget, sqrtPriceCurrent, liquidity, true)
     : getAmount1Delta(sqrtPriceCurrent, sqrtPriceTarget, liquidity, true);
 
   const amountRemainingNet = amountLessFee(amountRemaining, feeRate);
 
   let sqrtPriceNext: BN;
   let amountIn:      BN;
   let amountOut:     BN;
 
   if (amountRemainingNet.gte(amountToTarget)) {
     // Full step — reach target tick
     sqrtPriceNext = sqrtPriceTarget;
     amountIn      = amountToTarget;
     amountOut     = zeroForOne
       ? getAmount1Delta(sqrtPriceTarget, sqrtPriceCurrent, liquidity, false)
       : getAmount0Delta(sqrtPriceTarget, sqrtPriceCurrent, liquidity, false);
   } else {
     // Partial step — stop partway
     sqrtPriceNext = zeroForOne
       ? getNextSqrtPriceFromAmount0(sqrtPriceCurrent, liquidity, amountRemainingNet, true)
       : getNextSqrtPriceFromAmount1(sqrtPriceCurrent, liquidity, amountRemainingNet, true);
 
     amountIn  = zeroForOne
       ? getAmount0Delta(sqrtPriceNext, sqrtPriceCurrent, liquidity, true)
       : getAmount1Delta(sqrtPriceCurrent, sqrtPriceNext, liquidity, true);
 
     amountOut = zeroForOne
       ? getAmount1Delta(sqrtPriceNext, sqrtPriceCurrent, liquidity, false)
       : getAmount0Delta(sqrtPriceCurrent, sqrtPriceNext, liquidity, false);
   }
 
   const feeAmount = amountRemaining.sub(amountIn);
 
   return { sqrtPriceNext, amountIn, amountOut, feeAmount };
 }
 
 // ─── Pool-level quote ─────────────────────────────────────────────────────────
 
 export interface TickBoundary {
   tick:         number;
   sqrtPrice:    BN;      // Q64.64
   liquidityNet: BN;      // signed (use negative values for upper ticks)
 }
 
 export interface QuoteResult {
   amountIn:       BN;
   amountOut:      BN;
   feeAmount:      BN;
   sqrtPriceAfter: BN;
   tickAfter:      number;
   tickCrossings:  number;
   priceImpactBps: number;
 }
 
 /**
  * Quote a swap off-chain. No transaction needed.
  *
  * USAGE:
  *   const quote = quoteSwap({
  *     sqrtPriceCurrent: new BN(pool.sqrtPrice),
  *     liquidityCurrent: new BN(pool.liquidity),
  *     tickCurrent:      pool.tickCurrent,
  *     feeRate:          new BN(pool.feeRate),
  *     zeroForOne:       true,
  *     amount:           new BN(1_000_000),
  *     sqrtPriceLimit:   new BN(...),
  *     initializedTicks: [...],
  *   });
  *
  *   // Pass quote.amountOut * 995 / 1000 as amount_out_minimum (0.5% slippage)
  */
 export function quoteSwap(params: {
   sqrtPriceCurrent: BN;
   liquidityCurrent: BN;
   tickCurrent:      number;
   feeRate:          BN;
   zeroForOne:       boolean;
   amount:           BN;
   sqrtPriceLimit:   BN;
   initializedTicks: TickBoundary[];
 }): QuoteResult {
   const { zeroForOne, amount, sqrtPriceLimit, feeRate, initializedTicks } = params;
 
   let sqrtPriceCurrent = params.sqrtPriceCurrent.clone();
   let liquidity        = params.liquidityCurrent.clone();
   let amountRemaining  = amount.clone();
   let totalAmountIn    = new BN(0);
   let totalAmountOut   = new BN(0);
   let totalFee         = new BN(0);
   let crossings        = 0;
   let tickCurrent      = params.tickCurrent;
 
   // Sort ticks in swap direction
   const sortedTicks = [...initializedTicks].sort((a, b) =>
     zeroForOne ? b.tick - a.tick : a.tick - b.tick
   );
   const relevantTicks = sortedTicks.filter(t =>
     zeroForOne ? t.tick < params.tickCurrent : t.tick > params.tickCurrent
   );
 
   let tickIndex = 0;
 
   while (amountRemaining.gtn(0) && crossings < MAX_TICK_CROSSINGS) {
     // Find next target
     let sqrtPriceTarget = sqrtPriceLimit.clone();
     if (tickIndex < relevantTicks.length) {
       sqrtPriceTarget = relevantTicks[tickIndex].sqrtPrice.clone();
     }
 
     // Enforce price limit
     if (zeroForOne && sqrtPriceTarget.lt(sqrtPriceLimit)) sqrtPriceTarget = sqrtPriceLimit;
     if (!zeroForOne && sqrtPriceTarget.gt(sqrtPriceLimit)) sqrtPriceTarget = sqrtPriceLimit;
 
     const step = computeSwapStep(
       sqrtPriceCurrent,
       sqrtPriceTarget,
       liquidity,
       amountRemaining,
       feeRate
     );
 
     amountRemaining = amountRemaining.sub(step.amountIn.add(step.feeAmount));
     totalAmountIn   = totalAmountIn.add(step.amountIn);
     totalAmountOut  = totalAmountOut.add(step.amountOut);
     totalFee        = totalFee.add(step.feeAmount);
     sqrtPriceCurrent = step.sqrtPriceNext;
 
     if (step.sqrtPriceNext.eq(sqrtPriceTarget) && tickIndex < relevantTicks.length) {
       const net = relevantTicks[tickIndex].liquidityNet;
       if (zeroForOne) {
         liquidity = net.gte(new BN(0))
           ? liquidity.sub(net)
           : liquidity.add(net.neg());
       } else {
         liquidity = net.gte(new BN(0))
           ? liquidity.add(net)
           : liquidity.sub(net.neg());
       }
       tickCurrent = zeroForOne
         ? relevantTicks[tickIndex].tick - 1
         : relevantTicks[tickIndex].tick;
       tickIndex++;
       crossings++;
 
       if (zeroForOne && sqrtPriceCurrent.lte(sqrtPriceLimit)) break;
       if (!zeroForOne && sqrtPriceCurrent.gte(sqrtPriceLimit)) break;
     } else {
       tickCurrent = tickAtSqrtPrice(sqrtPriceCurrent);
       break;
     }
   }
 
   // Price impact in basis points
   const priceBefore   = params.sqrtPriceCurrent.mul(params.sqrtPriceCurrent);
   const priceAfter    = sqrtPriceCurrent.mul(sqrtPriceCurrent);
   const impactNum     = priceBefore.gt(priceAfter)
     ? priceBefore.sub(priceAfter)
     : priceAfter.sub(priceBefore);
   const priceImpactBps = priceBefore.isZero()
     ? 0
     : impactNum.muln(10_000).div(priceBefore).toNumber();
 
   return {
     amountIn:       totalAmountIn,
     amountOut:      totalAmountOut,
     feeAmount:      totalFee,
     sqrtPriceAfter: sqrtPriceCurrent,
     tickAfter:      tickCurrent,
     tickCrossings:  crossings,
     priceImpactBps,
   };
 }
 
 // ─── Utility ──────────────────────────────────────────────────────────────────
 
 import { MIN_TICK, MAX_TICK } from "./tick_math";
 
 export function tickAtSqrtPrice(sqrtPriceQ64: BN): number {
   const intPart = sqrtPriceQ64.shrn(64);
   if (intPart.isZero()) return MIN_TICK;
   const buf = intPart.toArrayLike(Buffer, "be", 8);
   const val = buf.readUInt32BE(0) * 2 ** 32 + buf.readUInt32BE(4);
   const log2 = Math.floor(Math.log2(val));
   const tick  = log2 * 2 * 13328;
   return Math.max(MIN_TICK, Math.min(MAX_TICK, tick));
 }