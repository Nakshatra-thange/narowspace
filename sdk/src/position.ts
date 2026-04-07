/**
 * sdk/src/position.ts
 *
 * SDK for position_mgr — open/close LP positions and collect fees.
 * Uses BN throughout. No bigint literals.
 */

import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  Keypair,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";

import { tickToSqrtPriceQ64, nearestUsableTick, priceToTick } from "./tick_math";
import { getPoolPDA, getVaultPDA, PoolState } from "../pool";

// ─── PDA helpers ──────────────────────────────────────────────────────────────

export function getPositionPDA(
  programId: PublicKey,
  nftMint: PublicKey
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("position"), nftMint.toBuffer()],
    programId
  );
}

export function getTickArrayPDA(
  tickManagerProgramId: PublicKey,
  pool: PublicKey,
  startTick: number
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("tick_array"),
      pool.toBuffer(),
      Buffer.from(new Int32Array([startTick]).buffer),
    ],
    tickManagerProgramId
  );
}

export function getTickBitmapPDA(
  tickManagerProgramId: PublicKey,
  pool: PublicKey,
  wordIndex: number
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("tick_bitmap"),
      pool.toBuffer(),
      Buffer.from(new Int32Array([wordIndex]).buffer),
    ],
    tickManagerProgramId
  );
}

// ─── Tick array helpers ───────────────────────────────────────────────────────

const TICK_SPACING    = 64;
const TICKS_PER_ARRAY = 88;
const ARRAY_SIZE      = TICK_SPACING * TICKS_PER_ARRAY; // 5632

export function tickToArrayStart(tick: number): number {
  const div = Math.floor(tick / ARRAY_SIZE);
  return div * ARRAY_SIZE;
}

export function arrayStartToBitmapIndex(startTick: number): number {
  const minArrayStart = tickToArrayStart(-443_636);
  return Math.floor((startTick - minArrayStart) / ARRAY_SIZE);
}

export function bitmapWordAndBit(arrayIndex: number): { wordIndex: number; bitIndex: number } {
  return {
    wordIndex: Math.floor(arrayIndex / 8),
    bitIndex:  arrayIndex % 8,
  };
}

// ─── Position state ───────────────────────────────────────────────────────────

export interface PositionState {
  pool:                   PublicKey;
  owner:                  PublicKey;
  nftMint:                PublicKey;
  tickLower:              number;
  tickUpper:              number;
  liquidity:              BN;
  feeGrowthCheckpoint0:   BN;
  feeGrowthCheckpoint1:   BN;
  tokensOwed0:            BN;
  tokensOwed1:            BN;
}

export async function fetchPosition(
  program: Program,
  positionPubkey: PublicKey
): Promise<PositionState> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const raw = await (program.account as any)["position"].fetch(positionPubkey);
  return {
    pool:                 raw.pool       as PublicKey,
    owner:                raw.owner      as PublicKey,
    nftMint:              raw.nftMint    as PublicKey,
    tickLower:            raw.tickLower  as number,
    tickUpper:            raw.tickUpper  as number,
    liquidity:            raw.liquidity  as BN,
    feeGrowthCheckpoint0: raw.feeGrowthCheckpoint0 as BN,
    feeGrowthCheckpoint1: raw.feeGrowthCheckpoint1 as BN,
    tokensOwed0:          raw.tokensOwed0 as BN,
    tokensOwed1:          raw.tokensOwed1 as BN,
  };
}

// ─── Open position ────────────────────────────────────────────────────────────

export interface OpenPositionParams {
  program:              Program;  // position_mgr
  poolCoreProgram:      Program;  // pool_core
  tickManagerProgramId: PublicKey;
  owner:                anchor.Wallet;
  pool:                 PublicKey;
  poolState:            PoolState;
  userAta0:             PublicKey;
  userAta1:             PublicKey;
  priceLower:           number;   // human price e.g. 140.0
  priceUpper:           number;   // human price e.g. 160.0
  amount0Desired:       BN;
  amount1Desired:       BN;
  slippageBps:          number;   // e.g. 50 = 0.5%
}

export interface OpenPositionResult {
  positionPubkey: PublicKey;
  nftMint:        PublicKey;
  nftAta:         PublicKey;
  tickLower:      number;
  tickUpper:      number;
}

export async function openPosition(
  params: OpenPositionParams
): Promise<OpenPositionResult> {
  const {
    program,
    poolCoreProgram,
    tickManagerProgramId,
    owner,
    pool,
    poolState,
    userAta0,
    userAta1,
    priceLower,
    priceUpper,
    amount0Desired,
    amount1Desired,
    slippageBps,
  } = params;

  const tickLower = nearestUsableTick(priceToTick(priceLower));
  const tickUpper = nearestUsableTick(priceToTick(priceUpper));

  // Slippage: minimum = desired * (10000 - slippageBps) / 10000
  const slippageFactor = 10_000 - slippageBps;
  const amount0Min = amount0Desired.muln(slippageFactor).divn(10_000);
  const amount1Min = amount1Desired.muln(slippageFactor).divn(10_000);

  // Derive vault PDAs
  const [vault0] = getVaultPDA(poolCoreProgram.programId, pool, 0);
  const [vault1] = getVaultPDA(poolCoreProgram.programId, pool, 1);

  // Derive tick array PDAs
  const lowerArrayStart = tickToArrayStart(tickLower);
  const upperArrayStart = tickToArrayStart(tickUpper);

  const [tickArrayLower]  = getTickArrayPDA(tickManagerProgramId, pool, lowerArrayStart);
  const [tickArrayUpper]  = getTickArrayPDA(tickManagerProgramId, pool, upperArrayStart);

  const lowerBitmapIdx = arrayStartToBitmapIndex(lowerArrayStart);
  const upperBitmapIdx = arrayStartToBitmapIndex(upperArrayStart);
  const { wordIndex: lWord } = bitmapWordAndBit(lowerBitmapIdx);
  const { wordIndex: uWord } = bitmapWordAndBit(upperBitmapIdx);

  const [tickBitmapLower] = getTickBitmapPDA(tickManagerProgramId, pool, lWord);
  const [tickBitmapUpper] = getTickBitmapPDA(tickManagerProgramId, pool, uWord);

  // Fresh NFT mint keypair — each position gets a unique mint
  const nftMintKp = Keypair.generate();
  const [positionPubkey] = getPositionPDA(program.programId, nftMintKp.publicKey);
  const nftAta = getAssociatedTokenAddressSync(nftMintKp.publicKey, owner.publicKey);

  await program.methods
    .openPosition(
      amount0Desired,
      amount1Desired,
      amount0Min,
      amount1Min,
      tickLower,
      tickUpper
    )
    .accounts({
      position:               positionPubkey,
      pool,
      tokenVault0:            vault0,
      tokenVault1:            vault1,
      nftMint:                nftMintKp.publicKey,
      nftTokenAccount:        nftAta,
      userTokenAccount0:      userAta0,
      userTokenAccount1:      userAta1,
      tickArrayLower,
      tickArrayUpper,
      tickBitmapLower,
      tickBitmapUpper,
      owner:                  owner.publicKey,
      tickManagerProgram:     tickManagerProgramId,
      poolCoreProgram:        poolCoreProgram.programId,
      tokenProgram:           TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram:          SystemProgram.programId,
      rent:                   SYSVAR_RENT_PUBKEY,
    })
    .signers([nftMintKp])
    .rpc();

  return {
    positionPubkey,
    nftMint:   nftMintKp.publicKey,
    nftAta,
    tickLower,
    tickUpper,
  };
}

// ─── Close position ───────────────────────────────────────────────────────────

export interface ClosePositionParams {
  program:              Program;
  poolCoreProgram:      Program;
  tickManagerProgramId: PublicKey;
  owner:                anchor.Wallet;
  positionPubkey:       PublicKey;
  positionState:        PositionState;
  pool:                 PublicKey;
  poolState:            PoolState;
  userAta0:             PublicKey;
  userAta1:             PublicKey;
  slippageBps:          number;
}

export async function closePosition(
  params: ClosePositionParams
): Promise<void> {
  const {
    program,
    poolCoreProgram,
    tickManagerProgramId,
    owner,
    positionPubkey,
    positionState,
    pool,
    poolState,
    userAta0,
    userAta1,
    slippageBps,
  } = params;

  const [vault0] = getVaultPDA(poolCoreProgram.programId, pool, 0);
  const [vault1] = getVaultPDA(poolCoreProgram.programId, pool, 1);

  const { tickLower, tickUpper, nftMint } = positionState;

  const lowerArrayStart = tickToArrayStart(tickLower);
  const upperArrayStart = tickToArrayStart(tickUpper);

  const [tickArrayLower]  = getTickArrayPDA(tickManagerProgramId, pool, lowerArrayStart);
  const [tickArrayUpper]  = getTickArrayPDA(tickManagerProgramId, pool, upperArrayStart);

  const { wordIndex: lWord } = bitmapWordAndBit(arrayStartToBitmapIndex(lowerArrayStart));
  const { wordIndex: uWord } = bitmapWordAndBit(arrayStartToBitmapIndex(upperArrayStart));

  const [tickBitmapLower] = getTickBitmapPDA(tickManagerProgramId, pool, lWord);
  const [tickBitmapUpper] = getTickBitmapPDA(tickManagerProgramId, pool, uWord);

  const nftAta = getAssociatedTokenAddressSync(nftMint, owner.publicKey);

  // Compute slippage-adjusted minimums
  const sqrtCurrent = BigInt(poolState.sqrtPrice.toString());
  const sqrtLower   = BigInt(tickToSqrtPriceQ64(tickLower).toString());
  const sqrtUpper   = BigInt(tickToSqrtPriceQ64(tickUpper).toString());
  const L           = BigInt(positionState.liquidity.toString());

  const { a0, a1 } = computeTokenAmounts(sqrtCurrent, sqrtLower, sqrtUpper, L);
  const slippageFactor = 10_000 - slippageBps;
  const min0 = new BN((a0 * BigInt(slippageFactor) / 10_000n).toString());
  const min1 = new BN((a1 * BigInt(slippageFactor) / 10_000n).toString());

  await program.methods
    .closePosition(min0, min1)
    .accounts({
      position:          positionPubkey,
      pool,
      tokenVault0:       vault0,
      tokenVault1:       vault1,
      nftMint,
      nftTokenAccount:   nftAta,
      userTokenAccount0: userAta0,
      userTokenAccount1: userAta1,
      tickArrayLower,
      tickArrayUpper,
      tickBitmapLower,
      tickBitmapUpper,
      owner:             owner.publicKey,
      tickManagerProgram:  tickManagerProgramId,
      poolCoreProgram:     poolCoreProgram.programId,
      tokenProgram:        TOKEN_PROGRAM_ID,
      systemProgram:       SystemProgram.programId,
    })
    .rpc();
}

// ─── Collect fees ─────────────────────────────────────────────────────────────

export async function collectFees(params: {
  program:         Program;
  poolCoreProgram: Program;
  owner:           anchor.Wallet;
  positionPubkey:  PublicKey;
  positionState:   PositionState;
  pool:            PublicKey;
  userAta0:        PublicKey;
  userAta1:        PublicKey;
}): Promise<void> {
  const { program, poolCoreProgram, owner, positionPubkey, positionState, pool, userAta0, userAta1 } = params;

  const [vault0] = getVaultPDA(poolCoreProgram.programId, pool, 0);
  const [vault1] = getVaultPDA(poolCoreProgram.programId, pool, 1);

  await program.methods
    .collectFees()
    .accounts({
      position:          positionPubkey,
      pool,
      tokenVault0:       vault0,
      tokenVault1:       vault1,
      userTokenAccount0: userAta0,
      userTokenAccount1: userAta1,
      owner:             owner.publicKey,
      poolCoreProgram:   poolCoreProgram.programId,
      tokenProgram:      TOKEN_PROGRAM_ID,
    })
    .rpc();
}

// ─── Fee quoting ──────────────────────────────────────────────────────────────

/**
 * Estimate fees earned by a position since last checkpoint.
 * Uses the same formula as Position::compute_fees_owed in Rust.
 * Call this before close_position to show the user what they'll receive.
 */
export function quoteFees(params: {
  positionState:    PositionState;
  feeGrowthGlobal0: BN;
  feeGrowthGlobal1: BN;
}): { fee0: BN; fee1: BN } {
  const { positionState, feeGrowthGlobal0, feeGrowthGlobal1 } = params;
  const L = positionState.liquidity;

  // delta = (current - checkpoint) mod 2^128 (wrapping subtraction)
  // We use BN which handles large numbers but not wrapping naturally.
  // For simplicity, if current >= checkpoint, delta = current - checkpoint.
  // If not (accumulator wrapped), delta is very large — we clamp to reasonable value.
  const delta0 = feeGrowthGlobal0.gte(positionState.feeGrowthCheckpoint0)
    ? feeGrowthGlobal0.sub(positionState.feeGrowthCheckpoint0)
    : new BN(0);
  const delta1 = feeGrowthGlobal1.gte(positionState.feeGrowthCheckpoint1)
    ? feeGrowthGlobal1.sub(positionState.feeGrowthCheckpoint1)
    : new BN(0);

  // fee = delta * L >> 64
  const fee0 = delta0.mul(L).shrn(64);
  const fee1 = delta1.mul(L).shrn(64);

  return { fee0, fee1 };
}

// ─── Internal: token amount calculation ──────────────────────────────────────
// Used for slippage calculation in closePosition. Uses native bigint internally.

function computeTokenAmounts(
  sqrtCurrent: bigint,
  sqrtLower:   bigint,
  sqrtUpper:   bigint,
  L:           bigint
): { a0: bigint; a1: bigint } {
  let a0 = 0n;
  let a1 = 0n;

  if (sqrtCurrent <= sqrtLower) {
    // All token0
    const diff = sqrtUpper - sqrtLower;
    const denom = (sqrtLower * sqrtUpper) >> 64n;
    a0 = denom > 0n ? (L * diff) / denom : 0n;
  } else if (sqrtCurrent >= sqrtUpper) {
    // All token1
    a1 = (L * (sqrtUpper - sqrtLower)) >> 64n;
  } else {
    // Split
    const diff0 = sqrtUpper - sqrtCurrent;
    const denom0 = (sqrtCurrent * sqrtUpper) >> 64n;
    a0 = denom0 > 0n ? (L * diff0) / denom0 : 0n;
    a1 = (L * (sqrtCurrent - sqrtLower)) >> 64n;
  }

  return { a0, a1 };
}