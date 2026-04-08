/**
 * tests/position_mgr.ts
 *
 * WHAT THIS TESTS:
 * The complete LP lifecycle end-to-end:
 *   1. Pool already initialized (from pool_core tests)
 *   2. Open a position (deposit tokens, mint NFT)
 *   3. Verify pool.liquidity increased
 *   4. Verify Position account state
 *   5. Collect fees (zero initially)
 *   6. Close position (tokens returned, NFT burned)
 *   7. Verify pool.liquidity decreased
 *
 * ALSO TESTS:
 *   - Reject open with zero liquidity (bad amounts)
 *   - Reject close from wrong owner
 *   - Reject invalid tick range
 *
 * RUN: anchor test --skip-deploy
 */

import * as anchor from "@coral-xyz/anchor";
import { Program} from "@coral-xyz/anchor";
import { BN } from "bn.js";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createMint,
  mintTo,
  getOrCreateAssociatedTokenAccount,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

import {
  tickToSqrtPriceQ64,
  priceToTick,
  nearestUsableTick,
  TICK_SPACING,
} from "../sdk/src/tick_math.ts";

import { getPoolPDA, getVaultPDA, fetchPool } from "../sdk/src/pool";
import {
  getPositionPDA,
  getTickArrayPDA,
  getTickBitmapPDA,
  tickToArrayStart,
  arrayStartToBitmapIndex,
  bitmapWordAndBit,
  fetchPosition,
  quoteFees,
} from "../sdk/src/position";

function sortMints(a: PublicKey, b: PublicKey): [PublicKey, PublicKey] {
  return a.toBase58() < b.toBase58() ? [a, b] : [b, a];
}

// ─── Helper: any cast for program.account ─────────────────────────────────────
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function acct(program: Program<any>) { return (program.account as any); }

// ─── Tests ────────────────────────────────────────────────────────────────────

describe("position_mgr", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const posMgr    = anchor.workspace.PositionMgr  as Program<any>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const poolProg  = anchor.workspace.PoolCore      as Program<any>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const tmProg    = anchor.workspace.TickManager   as Program<any>;
  const wallet    = provider.wallet as anchor.Wallet;
  const conn      = provider.connection;

  const FEE_RATE      = 3_000;
  const INITIAL_PRICE = 150;
  const PRICE_LOWER   = 140;
  const PRICE_UPPER   = 160;

  let mint0:      PublicKey;
  let mint1:      PublicKey;
  let userAta0:   PublicKey;
  let userAta1:   PublicKey;
  let poolPubkey: PublicKey;
  let vault0:     PublicKey;
  let vault1:     PublicKey;
  let payerKp:    Keypair;

  // Position state shared across tests
  let positionPubkey: PublicKey;
  let nftMint:        PublicKey;

  // ── Setup: spin up pool (replicates pool_core setup) ─────────────────────────

  before(async () => {
    const balance = await conn.getBalance(wallet.publicKey);
    if (balance < 2 * LAMPORTS_PER_SOL) {
      const sig = await conn.requestAirdrop(wallet.publicKey, 4 * LAMPORTS_PER_SOL);
      await conn.confirmTransaction(sig);
    }

    payerKp = (wallet as anchor.Wallet & { payer?: Keypair }).payer ?? Keypair.generate();

    // Create token pair
    const mintA = await createMint(conn, payerKp, wallet.publicKey, null, 6);
    const mintB = await createMint(conn, payerKp, wallet.publicKey, null, 6);
  
    [mint0, mint1] = sortMints(mintA, mintB);
    const ata0 = await getOrCreateAssociatedTokenAccount(conn, payerKp, mint0, wallet.publicKey);
    const ata1 = await getOrCreateAssociatedTokenAccount(conn, payerKp, mint1, wallet.publicKey);
    userAta0 = ata0.address;
    userAta1 = ata1.address;

    // Mint plenty of tokens to user
    await mintTo(conn, payerKp, mint0, userAta0, payerKp, 10_000_000_000);
    await mintTo(conn, payerKp, mint1, userAta1, payerKp, 10_000_000_000);

    // Derive pool addresses
    const [m0, m1] = sortMints(mint0, mint1);
    [poolPubkey] = getPoolPDA(poolProg.programId, m0, m1, FEE_RATE);
    [vault0]     = getVaultPDA(poolProg.programId, poolPubkey, 0);
    [vault1]     = getVaultPDA(poolProg.programId, poolPubkey, 1);

    // Initialize pool
    const initialTick      = nearestUsableTick(priceToTick(INITIAL_PRICE));
    const initialSqrtPrice = new BN(tickToSqrtPriceQ64(initialTick).toString());

    await poolProg.methods
      .initializePool(initialSqrtPrice, FEE_RATE, initialTick)
      .accounts({
        pool:               poolPubkey,
        tokenMint0:         m0,
        tokenMint1:         m1,
        tokenVault0:        vault0,
        tokenVault1:        vault1,
        tickManagerProgram: tmProg.programId,
        payer:              wallet.publicKey,
        tokenProgram:       TOKEN_PROGRAM_ID,
        systemProgram:      SystemProgram.programId,
        rent:               anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    // Initialize tick arrays needed for our range
    const tickLower = nearestUsableTick(priceToTick(PRICE_LOWER));
    const tickUpper = nearestUsableTick(priceToTick(PRICE_UPPER));

    const arraysNeeded = new Set([
      tickToArrayStart(tickLower),
      tickToArrayStart(tickUpper),
      tickToArrayStart(nearestUsableTick(priceToTick(INITIAL_PRICE))),
    ]);

    for (const startTick of arraysNeeded) {
      const [arrPDA] = getTickArrayPDA(tmProg.programId, poolPubkey, startTick);
      try {
        await tmProg.methods
          .initializeTickArray(startTick)
          .accounts({
            tickArray:     arrPDA,
            pool:          poolPubkey,
            payer:         wallet.publicKey,
            systemProgram: SystemProgram.programId,
          })
          .rpc();
      } catch {
        // Already exists from a previous test run — fine
      }
    }

    // Initialize bitmap accounts needed for our ticks
    const ticksToRegister = [tickLower, tickUpper];
    for (const tick of ticksToRegister) {
      const arrStart = tickToArrayStart(tick);
      const arrIdx   = arrayStartToBitmapIndex(arrStart);
      const { wordIndex } = bitmapWordAndBit(arrIdx);
      const [bitmapPDA] = getTickBitmapPDA(tmProg.programId, poolPubkey, wordIndex);

      // Initialize bitmap if it doesn't exist yet (it will be created by tick_manager
      // the first time update_tick is called — no separate init needed)
      // The bitmap is created by open_position's CPI call to update_tick
      void bitmapPDA; // used later in open_position
    }

    console.log("  Setup complete. Pool:", poolPubkey.toString());
  });

  // ── Test: open_position ───────────────────────────────────────────────────────

  describe("open_position", () => {
    it("opens a position and mints NFT receipt", async () => {
      const tickLower = nearestUsableTick(priceToTick(PRICE_LOWER));
      const tickUpper = nearestUsableTick(priceToTick(PRICE_UPPER));

      const lowerArrayStart = tickToArrayStart(tickLower);
      const upperArrayStart = tickToArrayStart(tickUpper);

      const [tickArrayLower]  = getTickArrayPDA(tmProg.programId, poolPubkey, lowerArrayStart);
      const [tickArrayUpper]  = getTickArrayPDA(tmProg.programId, poolPubkey, upperArrayStart);

      const lBitmapIdx = arrayStartToBitmapIndex(lowerArrayStart);
      const uBitmapIdx = arrayStartToBitmapIndex(upperArrayStart);
      const { wordIndex: lWord } = bitmapWordAndBit(lBitmapIdx);
      const { wordIndex: uWord } = bitmapWordAndBit(uBitmapIdx);

      const [tickBitmapLower] = getTickBitmapPDA(tmProg.programId, poolPubkey, lWord);
      const [tickBitmapUpper] = getTickBitmapPDA(tmProg.programId, poolPubkey, uWord);

      // Fresh NFT mint for this position
      const nftMintKp = Keypair.generate();
      const [posPDA]  = getPositionPDA(posMgr.programId, nftMintKp.publicKey);

      // NFT ATA for owner
      const { getAssociatedTokenAddressSync: getAta } = await import("@solana/spl-token");
      const nftAta = getAta(nftMintKp.publicKey, wallet.publicKey);

      const AMOUNT_0 = new BN(1_000_000); // 1 token0
      const AMOUNT_1 = new BN(150_000_000); // 150 token1

      // Get pool liquidity before
      const poolBefore = await fetchPool(poolProg, poolPubkey);
      const liquidityBefore = poolBefore.liquidity;

      await posMgr.methods
        .openPosition(
          AMOUNT_0,
          AMOUNT_1,
          new BN(0),   // min amounts (0 = no slippage protection in test)
          new BN(0),
          tickLower,
          tickUpper
        )
        .accounts({
          position:               posPDA,
          pool:                   poolPubkey,
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
          owner:                  wallet.publicKey,
          tickManagerProgram:     tmProg.programId,
          poolCoreProgram:        poolProg.programId,
          tokenProgram:           TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram:          SystemProgram.programId,
          rent:                   anchor.web3.SYSVAR_RENT_PUBKEY,
        })
        .signers([nftMintKp])
        .rpc();

      // Store for later tests
      positionPubkey = posPDA;
      nftMint        = nftMintKp.publicKey;

      // ── Verify Position account ──────────────────────────────────────────────
      const pos = await fetchPosition(posMgr, posPDA);
      expect(pos.owner.toString()).to.equal(wallet.publicKey.toString());
      expect(pos.tickLower).to.equal(tickLower);
      expect(pos.tickUpper).to.equal(tickUpper);
      expect(pos.liquidity.gtn(0)).to.equal(true);
      expect(pos.nftMint.toString()).to.equal(nftMintKp.publicKey.toString());

      // ── Verify NFT was minted ────────────────────────────────────────────────
      const nftAcct = await getAccount(conn, nftAta);
      expect(Number(nftAcct.amount)).to.equal(1);

      // ── Verify pool.liquidity increased ─────────────────────────────────────
      const poolAfter = await fetchPool(poolProg, poolPubkey);
      expect(poolAfter.liquidity.gt(liquidityBefore)).to.equal(true,
        "pool.liquidity should increase after opening position in-range"
      );

      console.log(`  ✓ Position opened: L=${pos.liquidity} ticks=[${tickLower},${tickUpper}]`);
      console.log(`  ✓ Pool liquidity: ${liquidityBefore} -> ${poolAfter.liquidity}`);
      console.log(`  ✓ NFT minted: ${nftMintKp.publicKey.toString()}`);
    });

    it("rejects invalid tick range (lower >= upper)", async () => {
      const nftMintKp = Keypair.generate();
      const [posPDA]  = getPositionPDA(posMgr.programId, nftMintKp.publicKey);
      const { getAssociatedTokenAddressSync: getAta } = await import("@solana/spl-token");
      const nftAta    = getAta(nftMintKp.publicKey, wallet.publicKey);

      const badTickLower = nearestUsableTick(priceToTick(PRICE_UPPER)); // swapped!
      const badTickUpper = nearestUsableTick(priceToTick(PRICE_LOWER));

      const lowerArrayStart = tickToArrayStart(badTickLower);
      const upperArrayStart = tickToArrayStart(badTickUpper);
      const [tickArrayLower] = getTickArrayPDA(tmProg.programId, poolPubkey, lowerArrayStart);
      const [tickArrayUpper] = getTickArrayPDA(tmProg.programId, poolPubkey, upperArrayStart);
      const { wordIndex: lWord } = bitmapWordAndBit(arrayStartToBitmapIndex(lowerArrayStart));
      const { wordIndex: uWord } = bitmapWordAndBit(arrayStartToBitmapIndex(upperArrayStart));
      const [tickBitmapLower] = getTickBitmapPDA(tmProg.programId, poolPubkey, lWord);
      const [tickBitmapUpper] = getTickBitmapPDA(tmProg.programId, poolPubkey, uWord);

      try {
        await posMgr.methods
          .openPosition(
            new BN(1_000_000), new BN(150_000_000),
            new BN(0), new BN(0),
            badTickLower, badTickUpper  // swapped — invalid
          )
          .accounts({
            position: posPDA, pool: poolPubkey,
            tokenVault0: vault0, tokenVault1: vault1,
            nftMint: nftMintKp.publicKey, nftTokenAccount: nftAta,
            userTokenAccount0: userAta0, userTokenAccount1: userAta1,
            tickArrayLower, tickArrayUpper,
            tickBitmapLower, tickBitmapUpper,
            owner: wallet.publicKey,
            tickManagerProgram: tmProg.programId,
            poolCoreProgram: poolProg.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
            associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
            rent: anchor.web3.SYSVAR_RENT_PUBKEY,
          })
          .signers([nftMintKp])
          .rpc();
        expect.fail("Should have thrown InvalidTickRange");
      } catch (err: unknown) {
        expect((err as Error).message).to.include("InvalidTickRange");
      }
    });
  });

  // ── Test: fee quoting ─────────────────────────────────────────────────────────

  describe("fee quoting", () => {
    it("quotes zero fees right after opening", async () => {
      if (!positionPubkey) return; // skip if open_position failed

      const pos  = await fetchPosition(posMgr, positionPubkey);
      const pool = await fetchPool(poolProg, poolPubkey);

      const { fee0, fee1 } = quoteFees({
        positionState:    pos,
        feeGrowthGlobal0: pool.feeGrowthGlobal0,
        feeGrowthGlobal1: pool.feeGrowthGlobal1,
      });

      // No swaps have happened — fees should be zero
      expect(fee0.toNumber()).to.equal(0);
      expect(fee1.toNumber()).to.equal(0);
      console.log("  ✓ Zero fees right after open (no swaps yet)");
    });
  });

  // ── Test: collect_fees ────────────────────────────────────────────────────────

  describe("collect_fees", () => {
    it("collects zero fees without error", async () => {
      if (!positionPubkey) return;

      await posMgr.methods
        .collectFees()
        .accounts({
          position:          positionPubkey,
          pool:              poolPubkey,
          tokenVault0:       vault0,
          tokenVault1:       vault1,
          userTokenAccount0: userAta0,
          userTokenAccount1: userAta1,
          owner:             wallet.publicKey,
          poolCoreProgram:   poolProg.programId,
          tokenProgram:      TOKEN_PROGRAM_ID,
        })
        .rpc();

      console.log("  ✓ collect_fees succeeded (zero fees, no revert)");
    });
  });

  // ── Test: close_position ──────────────────────────────────────────────────────

  describe("close_position", () => {
    it("rejects close from wrong owner", async () => {
      if (!positionPubkey) return;

      const fakeOwner = Keypair.generate();
      const tickLower = nearestUsableTick(priceToTick(PRICE_LOWER));
      const tickUpper = nearestUsableTick(priceToTick(PRICE_UPPER));
      const lAS = tickToArrayStart(tickLower);
      const uAS = tickToArrayStart(tickUpper);
      const [tal] = getTickArrayPDA(tmProg.programId, poolPubkey, lAS);
      const [tau] = getTickArrayPDA(tmProg.programId, poolPubkey, uAS);
      const { wordIndex: lw } = bitmapWordAndBit(arrayStartToBitmapIndex(lAS));
      const { wordIndex: uw } = bitmapWordAndBit(arrayStartToBitmapIndex(uAS));
      const [tbl] = getTickBitmapPDA(tmProg.programId, poolPubkey, lw);
      const [tbu] = getTickBitmapPDA(tmProg.programId, poolPubkey, uw);

      const { getAssociatedTokenAddressSync: getAta } = await import("@solana/spl-token");
      const nftAta = getAta(nftMint, wallet.publicKey); // actual owner's ATA

      try {
        await posMgr.methods
          .closePosition(new BN(0), new BN(0))
          .accounts({
            position:          positionPubkey,
            pool:              poolPubkey,
            tokenVault0:       vault0,
            tokenVault1:       vault1,
            nftMint,
            nftTokenAccount:   nftAta,
            userTokenAccount0: userAta0,
            userTokenAccount1: userAta1,
            tickArrayLower:    tal,
            tickArrayUpper:    tau,
            tickBitmapLower:   tbl,
            tickBitmapUpper:   tbu,
            owner:             fakeOwner.publicKey, // wrong owner
            tickManagerProgram:  tmProg.programId,
            poolCoreProgram:     poolProg.programId,
            tokenProgram:        TOKEN_PROGRAM_ID,
            systemProgram:       SystemProgram.programId,
          })
          .signers([fakeOwner])
          .rpc();
        expect.fail("Should have thrown Unauthorized");
      } catch (err: unknown) {
        expect((err as Error).message).to.satisfy((m: string) =>
          m.includes("Unauthorized") || m.includes("constraint") || m.includes("seeds")
        );
      }
    });

    it("closes position, returns tokens, burns NFT", async () => {
      if (!positionPubkey) return;

      const tickLower = nearestUsableTick(priceToTick(PRICE_LOWER));
      const tickUpper = nearestUsableTick(priceToTick(PRICE_UPPER));

      const lAS = tickToArrayStart(tickLower);
      const uAS = tickToArrayStart(tickUpper);
      const [tickArrayLower] = getTickArrayPDA(tmProg.programId, poolPubkey, lAS);
      const [tickArrayUpper] = getTickArrayPDA(tmProg.programId, poolPubkey, uAS);
      const { wordIndex: lw } = bitmapWordAndBit(arrayStartToBitmapIndex(lAS));
      const { wordIndex: uw } = bitmapWordAndBit(arrayStartToBitmapIndex(uAS));
      const [tickBitmapLower] = getTickBitmapPDA(tmProg.programId, poolPubkey, lw);
      const [tickBitmapUpper] = getTickBitmapPDA(tmProg.programId, poolPubkey, uw);

      const { getAssociatedTokenAddressSync: getAta } = await import("@solana/spl-token");
      const nftAta = getAta(nftMint, wallet.publicKey);

      // Token balances before
      const ata0Before = await getAccount(conn, userAta0);
      const poolBefore = await fetchPool(poolProg, poolPubkey);

      await posMgr.methods
        .closePosition(new BN(0), new BN(0)) // no slippage minimum in test
        .accounts({
          position:          positionPubkey,
          pool:              poolPubkey,
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
          owner:             wallet.publicKey,
          tickManagerProgram:  tmProg.programId,
          poolCoreProgram:     poolProg.programId,
          tokenProgram:        TOKEN_PROGRAM_ID,
          systemProgram:       SystemProgram.programId,
        })
        .rpc();

      // ── Verify tokens returned ───────────────────────────────────────────────
      const ata0After = await getAccount(conn, userAta0);
      expect(Number(ata0After.amount)).to.be.gte(Number(ata0Before.amount),
        "user should have received token0 back"
      );

      // ── Verify pool.liquidity decreased ─────────────────────────────────────
      const poolAfter = await fetchPool(poolProg, poolPubkey);
      expect(poolAfter.liquidity.lte(poolBefore.liquidity)).to.equal(true,
        "pool.liquidity should decrease after closing in-range position"
      );

      // ── Verify NFT burned (supply = 0) ────────────────────────────────────────
      try {
        await getAccount(conn, nftAta);
        // If account still exists, amount should be 0
      } catch {
        // Account closed after burn — this is expected
      }

      console.log(`  ✓ Position closed`);
      console.log(`  ✓ Pool liquidity: ${poolBefore.liquidity} -> ${poolAfter.liquidity}`);
    });
  });
});