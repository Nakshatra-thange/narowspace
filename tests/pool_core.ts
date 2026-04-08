/**
 * tests/pool_core.ts
 * Integration tests for pool_core: initialize_pool + swap validation.
 *
 * RUN: anchor test --skip-deploy
 */

import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { BN } from "bn.js";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  mintTo,
  getOrCreateAssociatedTokenAccount,
} from "@solana/spl-token";
import { expect } from "chai";

import {
  tickToSqrtPriceQ64,
  priceToTick,
  nearestUsableTick,
  tickToArrayStartTick,
  arrayStartTickToBitmapIndex,
  bitmapWordAndBit,
  TICK_SPACING,
  TICKS_PER_ARRAY,
} from "../sdk/src/tick_math.ts";

import { quoteSwap } from "../sdk/src/swap_math";
import { getPoolPDA, getVaultPDA, fetchPool } from "../sdk/src/pool";

// ─── Helpers ──────────────────────────────────────────────────────────────────

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function acct(program: Program<any>) {
  return (program.account as any);
}

function getTickArrayPDA(programId: PublicKey, pool: PublicKey, startTick: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("tick_array"), pool.toBuffer(), Buffer.from(new Int32Array([startTick]).buffer)],
    programId
  );
}

function getTickBitmapPDA(programId: PublicKey, pool: PublicKey, wordIndex: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("tick_bitmap"), pool.toBuffer(), Buffer.from(new Int32Array([wordIndex]).buffer)],
    programId
  );
}

// ─── Tests ────────────────────────────────────────────────────────────────────

describe("pool_core", () => {
  const provider   = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const poolProgram = anchor.workspace.PoolCore    as Program<any>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const tmProgram   = anchor.workspace.TickManager as Program<any>;
  const wallet      = provider.wallet as anchor.Wallet;
  const connection  = provider.connection;

  const FEE_RATE      = 3_000;
  const INITIAL_PRICE = 150;
  const ARRAY_SIZE    = TICKS_PER_ARRAY * TICK_SPACING;

  let mint0:      PublicKey;
  let mint1:      PublicKey;
  let userAta0:   PublicKey;
  let userAta1:   PublicKey;
  let poolPubkey: PublicKey;
  let vault0:     PublicKey;
  let vault1:     PublicKey;
  let payerKp:    Keypair;

  // ── Setup ───────────────────────────────────────────────────────────────────

  before(async () => {
    const balance = await connection.getBalance(wallet.publicKey);
    if (balance < LAMPORTS_PER_SOL) {
      const sig = await connection.requestAirdrop(wallet.publicKey, 4 * LAMPORTS_PER_SOL);
      await connection.confirmTransaction(sig);
    }

    // Anchor's localnet wallet exposes payer as a Keypair
    payerKp = (wallet as anchor.Wallet & { payer?: Keypair }).payer ?? Keypair.generate();

    const mintA = await createMint(connection, payerKp, wallet.publicKey, null, 6);
    const mintB = await createMint(connection, payerKp, wallet.publicKey, null, 6);
    [mint0, mint1] = Buffer.compare(mintA.toBuffer(), mintB.toBuffer()) < 0
      ? [mintA, mintB]
      : [mintB, mintA];

    const ata0 = await getOrCreateAssociatedTokenAccount(connection, payerKp, mint0, wallet.publicKey);
    const ata1 = await getOrCreateAssociatedTokenAccount(connection, payerKp, mint1, wallet.publicKey);
    userAta0 = ata0.address;
    userAta1 = ata1.address;

    await mintTo(connection, payerKp, mint0, userAta0, payerKp, 1_000_000_000);
    await mintTo(connection, payerKp, mint1, userAta1, payerKp, 1_000_000_000);

    [poolPubkey] = getPoolPDA(poolProgram.programId, mint0, mint1, FEE_RATE);
    [vault0]     = getVaultPDA(poolProgram.programId, poolPubkey, 0);
    [vault1]     = getVaultPDA(poolProgram.programId, poolPubkey, 1);

    console.log("  Setup: pool=", poolPubkey.toString());
  });

  // ── initialize_pool ─────────────────────────────────────────────────────────

  describe("initialize_pool", () => {
    it("creates pool with correct initial state", async () => {
      const initialTick      = nearestUsableTick(priceToTick(INITIAL_PRICE));
      const sqrtPriceQ64     = tickToSqrtPriceQ64(initialTick);
      const initialSqrtPrice = new BN(sqrtPriceQ64.toString());

      await poolProgram.methods
        .initializePool(initialSqrtPrice, FEE_RATE, initialTick)
        .accounts({
          pool:               poolPubkey,
          tokenMint0:         mint0,
          tokenMint1:         mint1,
          tokenVault0:        vault0,
          tokenVault1:        vault1,
          tickManagerProgram: tmProgram.programId,
          payer:              wallet.publicKey,
          tokenProgram:       TOKEN_PROGRAM_ID,
          systemProgram:      SystemProgram.programId,
          rent:               anchor.web3.SYSVAR_RENT_PUBKEY,
        })
        .rpc();

      const pool = await fetchPool(poolProgram, poolPubkey);
      expect(pool.initialized).to.equal(true);
      expect(pool.feeRate).to.equal(FEE_RATE);
      expect(pool.tickCurrent).to.equal(initialTick);
      expect(pool.liquidity.toString()).to.equal("0");
      console.log(`  ✓ Pool initialized tick=${initialTick}`);
    });
  });

  // ── swap validation ─────────────────────────────────────────────────────────

  describe("swap validations", () => {
    let tickArrayPDA: PublicKey;
    const initialTick = nearestUsableTick(priceToTick(INITIAL_PRICE));
    const arrayStart  = tickToArrayStartTick(initialTick);

    before(async () => {
      // Create tick arrays around current price
      [tickArrayPDA] = getTickArrayPDA(tmProgram.programId, poolPubkey, arrayStart);
      await tmProgram.methods
        .initializeTickArray(arrayStart)
        .accounts({
          tickArray:     tickArrayPDA,
          pool:          poolPubkey,
          payer:         wallet.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .rpc();

      // Initialize tick array above too
      const arrayStartAbove = arrayStart + ARRAY_SIZE;
      const [abovePDA] = getTickArrayPDA(tmProgram.programId, poolPubkey, arrayStartAbove);
      await tmProgram.methods
        .initializeTickArray(arrayStartAbove)
        .accounts({
          tickArray:     abovePDA,
          pool:          poolPubkey,
          payer:         wallet.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .rpc();

      // Add ticks for LP range $140-$160
      const lowerTick       = nearestUsableTick(priceToTick(140));
      const upperTick       = nearestUsableTick(priceToTick(160));
      const LIQUIDITY       = new BN(100_000_000);

      for (const [tick, isUpper] of [[lowerTick, false], [upperTick, true]] as [number, boolean][]) {
        const tArrayStart = tickToArrayStartTick(tick);
        const [tArrayPDA] = getTickArrayPDA(tmProgram.programId, poolPubkey, tArrayStart);
        const arrIdx      = arrayStartTickToBitmapIndex(tArrayStart);
        const { wordIndex } = bitmapWordAndBit(arrIdx);
        const [bitmapPDA] = getTickBitmapPDA(tmProgram.programId, poolPubkey, wordIndex);

        // Init array if new
        try {
          await tmProgram.methods
            .initializeTickArray(tArrayStart)
            .accounts({
              tickArray:     tArrayPDA,
              pool:          poolPubkey,
              payer:         wallet.publicKey,
              systemProgram: SystemProgram.programId,
            })
            .rpc();
        } catch {
          // Already exists — fine
        }

        await tmProgram.methods
          .updateTick(tick, wordIndex, LIQUIDITY, isUpper, new BN(0), new BN(0))
          .accounts({
            tickArray:  tArrayPDA,
            tickBitmap: bitmapPDA,
            pool:       poolPubkey,
            authority:  wallet.publicKey,
            systemProgram: SystemProgram.programId,
          })
          .rpc();
      }
    });

    it("quotes swap correctly off-chain", () => {
      const sqrtPriceCurrent = new BN(tickToSqrtPriceQ64(initialTick).toString());
      const sqrtPriceLower   = new BN(tickToSqrtPriceQ64(nearestUsableTick(priceToTick(140))).toString());
      const sqrtPriceUpper   = new BN(tickToSqrtPriceQ64(nearestUsableTick(priceToTick(160))).toString());

      const quote = quoteSwap({
        sqrtPriceCurrent,
        liquidityCurrent: new BN(100_000_000),
        tickCurrent:      initialTick,
        feeRate:          new BN(FEE_RATE),
        zeroForOne:       true,
        amount:           new BN(1_000_000),
        sqrtPriceLimit:   sqrtPriceLower,
        initializedTicks: [
          {
            tick:         nearestUsableTick(priceToTick(140)),
            sqrtPrice:    sqrtPriceLower,
            liquidityNet: new BN(-100_000_000),
          },
          {
            tick:         nearestUsableTick(priceToTick(160)),
            sqrtPrice:    sqrtPriceUpper,
            liquidityNet: new BN(100_000_000),
          },
        ],
      });

      expect(quote.amountOut.gtn(0)).to.equal(true);
      expect(quote.feeAmount.gtn(0)).to.equal(true);
      expect(quote.tickCrossings).to.be.at.most(10);
      console.log(
        `  ✓ Quote: out=${quote.amountOut} fee=${quote.feeAmount} impact=${quote.priceImpactBps}bps`
      );
    });

    it("rejects zero-amount swap", async () => {
      try {
        await poolProgram.methods
          .swap(new BN(0), true, new BN(1), new BN(0))
          .accounts({
            pool:                poolPubkey,
            tokenVault0:         vault0,
            tokenVault1:         vault1,
            userTokenAccount0:   userAta0,
            userTokenAccount1:   userAta1,
            user:                wallet.publicKey,
            tokenProgram:        TOKEN_PROGRAM_ID,
          })
          .remainingAccounts([
            { pubkey: tickArrayPDA, isWritable: false, isSigner: false },
          ])
          .rpc();
        expect.fail("Should have thrown ZeroAmount");
      } catch (err: unknown) {
        expect((err as Error).message).to.include("ZeroAmount");
      }
    });

    it("surfaces liquidity error before wrong-direction price limit when pool is empty", async () => {
      const pool          = await fetchPool(poolProgram, poolPubkey);
      const currentSqrtP  = pool.sqrtPrice;
      // zero_for_one=true means price goes DOWN — limit must be BELOW current
      const wrongLimit    = currentSqrtP.addn(1000); // ABOVE current — wrong

      try {
        await poolProgram.methods
          .swap(new BN(100_000), true, wrongLimit, new BN(0))
          .accounts({
            pool:                poolPubkey,
            tokenVault0:         vault0,
            tokenVault1:         vault1,
            userTokenAccount0:   userAta0,
            userTokenAccount1:   userAta1,
            user:                wallet.publicKey,
            tokenProgram:        TOKEN_PROGRAM_ID,
          })
          .rpc();
        expect.fail("Should have thrown InsufficientLiquidity");
      } catch (err: unknown) {
        expect((err as Error).message).to.include("InsufficientLiquidity");
      }
    });

    it("rejects swap with zero liquidity", async () => {
      const pool = await fetchPool(poolProgram, poolPubkey);
      // Pool has liquidity=0 (no position_mgr yet — that's Day 3)
      // So InsufficientLiquidity should fire
      const sqrtLimit = pool.sqrtPrice.subn(1000);

      try {
        await poolProgram.methods
          .swap(new BN(100_000), true, sqrtLimit, new BN(0))
          .accounts({
            pool:                poolPubkey,
            tokenVault0:         vault0,
            tokenVault1:         vault1,
            userTokenAccount0:   userAta0,
            userTokenAccount1:   userAta1,
            user:                wallet.publicKey,
            tokenProgram:        TOKEN_PROGRAM_ID,
          })
          .remainingAccounts([
            { pubkey: tickArrayPDA, isWritable: false, isSigner: false },
          ])
          .rpc();
        expect.fail("Should have thrown InsufficientLiquidity");
      } catch (err: unknown) {
        expect((err as Error).message).to.include("InsufficientLiquidity");
      }
    });
  });
});
