/**
 * pool.ts
 *
 * SDK for pool_core — initialize pools and read pool state.
 * Used by scripts and tests.
 */

import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  mintTo,
  getOrCreateAssociatedTokenAccount,
} from "@solana/spl-token";
import { tickToSqrtPriceQ64, priceToTick, nearestUsableTick } from "./tick_math";

function comparePubkeys(a: PublicKey, b: PublicKey): number {
  return Buffer.compare(a.toBuffer(), b.toBuffer());
}

export function getPoolPDA(
  programId: PublicKey,
  mint0: PublicKey,
  mint1: PublicKey,
  feeRate: number
): [PublicKey, number] {
  const [m0, m1] = comparePubkeys(mint0, mint1) < 0
    ? [mint0, mint1]
    : [mint1, mint0];

  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("pool"),
      m0.toBuffer(),
      m1.toBuffer(),
      Buffer.from(new Uint32Array([feeRate]).buffer),
    ],
    programId
  );
}

export function getVaultPDA(
  programId: PublicKey,
  poolPubkey: PublicKey,
  vaultIndex: 0 | 1
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from(`vault_${vaultIndex}`), poolPubkey.toBuffer()],
    programId
  );
}

export interface InitPoolParams {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  program: Program<any>;
  payer: anchor.Wallet;
  mint0: PublicKey;
  mint1: PublicKey;
  feeRate: number;
  initialPrice: number;
  tickManagerProgramId: PublicKey;
}

export interface InitPoolResult {
  poolPubkey: PublicKey;
  vault0Pubkey: PublicKey;
  vault1Pubkey: PublicKey;
  initialTick: number;
  initialSqrtPrice: BN;
}

export async function initializePool(params: InitPoolParams): Promise<InitPoolResult> {
  const { program, payer, feeRate, initialPrice, tickManagerProgramId } = params;

  let mint0 = params.mint0;
  let mint1 = params.mint1;
  if (comparePubkeys(mint0, mint1) > 0) {
    [mint0, mint1] = [mint1, mint0];
  }

  const initialTick = nearestUsableTick(priceToTick(initialPrice));
  const sqrtPriceQ64 = tickToSqrtPriceQ64(initialTick);
  const initialSqrtPrice = new BN(sqrtPriceQ64.toString());

  const [poolPubkey] = getPoolPDA(program.programId, mint0, mint1, feeRate);
  const [vault0Pubkey] = getVaultPDA(program.programId, poolPubkey, 0);
  const [vault1Pubkey] = getVaultPDA(program.programId, poolPubkey, 1);

  await program.methods
    .initializePool(initialSqrtPrice, feeRate, initialTick)
    .accounts({
      pool: poolPubkey,
      tokenMint0: mint0,
      tokenMint1: mint1,
      tokenVault0: vault0Pubkey,
      tokenVault1: vault1Pubkey,
      tickManagerProgram: tickManagerProgramId,
      payer: payer.publicKey,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
      rent: SYSVAR_RENT_PUBKEY,
    })
    .rpc();

  return { poolPubkey, vault0Pubkey, vault1Pubkey, initialTick, initialSqrtPrice };
}

export interface PoolState {
  tokenMint0: PublicKey;
  tokenMint1: PublicKey;
  tokenVault0: PublicKey;
  tokenVault1: PublicKey;
  sqrtPrice: BN;
  tickCurrent: number;
  liquidity: BN;
  feeRate: number;
  feeGrowthGlobal0: BN;
  feeGrowthGlobal1: BN;
  initialized: boolean;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export async function fetchPool(program: Program<any>, poolPubkey: PublicKey): Promise<PoolState> {
  const raw = await program.account.pool.fetch(poolPubkey);
  return {
    tokenMint0: raw.tokenMint0 as PublicKey,
    tokenMint1: raw.tokenMint1 as PublicKey,
    tokenVault0: raw.tokenVault0 as PublicKey,
    tokenVault1: raw.tokenVault1 as PublicKey,
    sqrtPrice: raw.sqrtPrice as BN,
    tickCurrent: raw.tickCurrent as number,
    liquidity: raw.liquidity as BN,
    feeRate: raw.feeRate as number,
    feeGrowthGlobal0: raw.feeGrowthGlobal0 as BN,
    feeGrowthGlobal1: raw.feeGrowthGlobal1 as BN,
    initialized: raw.initialized as boolean,
  };
}

export async function createTestTokenPair(
  connection: anchor.web3.Connection,
  payer: Keypair,
  amount0: number,
  amount1: number
): Promise<{
  mint0: PublicKey;
  mint1: PublicKey;
  userAta0: PublicKey;
  userAta1: PublicKey;
}> {
  const mintA = await createMint(connection, payer, payer.publicKey, null, 6);
  const mintB = await createMint(connection, payer, payer.publicKey, null, 6);

  const [mint0, mint1] = comparePubkeys(mintA, mintB) < 0
    ? [mintA, mintB]
    : [mintB, mintA];

  const ata0 = await getOrCreateAssociatedTokenAccount(
    connection, payer, mint0, payer.publicKey
  );
  const ata1 = await getOrCreateAssociatedTokenAccount(
    connection, payer, mint1, payer.publicKey
  );

  await mintTo(connection, payer, mint0, ata0.address, payer, amount0);
  await mintTo(connection, payer, mint1, ata1.address, payer, amount1);

  return {
    mint0,
    mint1,
    userAta0: ata0.address,
    userAta1: ata1.address,
  };
}
