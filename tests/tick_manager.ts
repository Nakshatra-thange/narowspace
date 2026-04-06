/**
 * tests/tick_manager.ts
 * Full integration test for all tick_manager instructions.
 * All tests must pass before Day 2 begins.
 *
 * RUN: anchor test --skip-deploy
 */

import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

import {
  tickToPrice,
  priceToTick,
  nearestUsableTick,
  tickToArrayStartTick,
  arrayStartTickToBitmapIndex,
  bitmapWordAndBit,
  TICK_SPACING,
  TICKS_PER_ARRAY,
} from "../sdk/src/tick_math";

// ─── Helpers ──────────────────────────────────────────────────────────────────

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function acct(program: Program<any>) {
  return (program.account as any);
}

function getTickArrayPDA(programId: PublicKey, pool: PublicKey, startTick: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("tick_array"),
      pool.toBuffer(),
      Buffer.from(new Int32Array([startTick]).buffer),
    ],
    programId
  );
}

function getTickBitmapPDA(programId: PublicKey, pool: PublicKey, wordIndex: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("tick_bitmap"),
      pool.toBuffer(),
      Buffer.from(new Int32Array([wordIndex]).buffer),
    ],
    programId
  );
}

// ─── Tests ────────────────────────────────────────────────────────────────────

describe("tick_manager", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const program  = anchor.workspace.TickManager as Program<any>;
  const wallet   = provider.wallet as anchor.Wallet;
  const pool     = Keypair.generate();
  const ARRAY_SIZE = TICKS_PER_ARRAY * TICK_SPACING;
  const START_TICK = 0;

  // ── Off-chain math ──────────────────────────────────────────────────────────

  describe("Off-chain math (TypeScript)", () => {
    it("tick 0 -> price 1.0", () => {
      expect(tickToPrice(0)).to.be.approximately(1.0, 0.000001);
    });

    it("tick 69082 -> price ~1000", () => {
      expect(tickToPrice(69082)).to.be.approximately(1000, 0.5);
    });

    it("priceToTick round-trips", () => {
      for (const price of [1, 10, 150, 1000]) {
        const tick = priceToTick(price);
        const recovered = tickToPrice(tick);
        expect(recovered).to.be.at.most(price * 1.001);
        expect(recovered).to.be.at.least(price * 0.999);
      }
    });

    it("nearestUsableTick snaps to TICK_SPACING multiples", () => {
      expect(nearestUsableTick(0)).to.equal(0);
      expect(nearestUsableTick(1)).to.equal(0);
      expect(nearestUsableTick(63)).to.equal(64);
      expect(nearestUsableTick(64)).to.equal(64);
      expect(nearestUsableTick(-1)).to.equal(0);
      expect(nearestUsableTick(-64)).to.equal(-64);
    });

    it("tickToArrayStartTick groups correctly", () => {
      expect(tickToArrayStartTick(0)).to.equal(0);
      expect(tickToArrayStartTick(ARRAY_SIZE - 1)).to.equal(0);
      expect(tickToArrayStartTick(ARRAY_SIZE)).to.equal(ARRAY_SIZE);
      expect(tickToArrayStartTick(-1)).to.equal(-ARRAY_SIZE);
    });

    it("bitmap word and bit calculations", () => {
      const idx0 = arrayStartTickToBitmapIndex(0);
      const idx1 = arrayStartTickToBitmapIndex(ARRAY_SIZE);
      const { wordIndex: w0, bitIndex: b0 } = bitmapWordAndBit(idx0);
      const { wordIndex: w1, bitIndex: b1 } = bitmapWordAndBit(idx1);
      expect(b0).to.equal(0);
      expect(b1).to.equal(1);
      expect(w0).to.equal(w1); // both in same word
    });
  });

  // ── On-chain: initialize_tick_array ────────────────────────────────────────

  describe("initialize_tick_array", () => {
    it("creates tick array at tick 0", async () => {
      const [tickArrayPDA] = getTickArrayPDA(program.programId, pool.publicKey, START_TICK);

      await program.methods
        .initializeTickArray(START_TICK)
        .accounts({
          tickArray:     tickArrayPDA,
          pool:          pool.publicKey,
          payer:         wallet.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .rpc();

      const account = await acct(program)["tickArray"].fetch(tickArrayPDA);
      expect(account.startTickIndex).to.equal(START_TICK);
      expect(account.pool.toString()).to.equal(pool.publicKey.toString());

      for (const tick of account.ticks) {
        expect(tick.initialized).to.equal(false);
      }
      console.log(`  ✓ TickArray PDA: ${tickArrayPDA.toString()}`);
    });

    it("rejects non-boundary start tick", async () => {
      const [badPDA] = getTickArrayPDA(program.programId, pool.publicKey, 100);
      try {
        await program.methods
          .initializeTickArray(100)
          .accounts({
            tickArray:     badPDA,
            pool:          pool.publicKey,
            payer:         wallet.publicKey,
            systemProgram: SystemProgram.programId,
          })
          .rpc();
        expect.fail("Should have thrown");
      } catch (err: unknown) {
        expect((err as Error).message).to.include("InvalidTickSpacing");
      }
    });
  });

  // ── On-chain: update_tick ───────────────────────────────────────────────────

  describe("update_tick", () => {
    const TICK_UPPER = ARRAY_SIZE; // 5632
    const LIQUIDITY  = new BN(1_000_000);

    before(async () => {
      // Init upper tick array
      const [upperArrayPDA] = getTickArrayPDA(program.programId, pool.publicKey, TICK_UPPER);
      await program.methods
        .initializeTickArray(TICK_UPPER)
        .accounts({
          tickArray:     upperArrayPDA,
          pool:          pool.publicKey,
          payer:         wallet.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .rpc();
    });

    it("lower tick gets positive liquidity_net", async () => {
      const [tickArrayPDA] = getTickArrayPDA(
        program.programId, pool.publicKey, tickToArrayStartTick(0)
      );
      const { wordIndex } = bitmapWordAndBit(arrayStartTickToBitmapIndex(tickToArrayStartTick(0)));
      const [bitmapPDA]   = getTickBitmapPDA(program.programId, pool.publicKey, wordIndex);

      await program.methods
        .updateTick(0, LIQUIDITY, false, new BN(0), new BN(0))
        .accounts({
          tickArray:  tickArrayPDA,
          tickBitmap: bitmapPDA,
          pool:       pool.publicKey,
          authority:  wallet.publicKey,
        })
        .rpc();

      const ta   = await acct(program)["tickArray"].fetch(tickArrayPDA);
      const tick = ta.ticks[0];
      expect(tick.initialized).to.equal(true);
      expect((tick.liquidityNet as BN).toString()).to.equal(LIQUIDITY.toString());
      console.log(`  ✓ Lower tick 0: initialized=true liquidityNet=+${LIQUIDITY}`);
    });

    it("upper tick gets negative liquidity_net", async () => {
      const upperArrayStart = tickToArrayStartTick(TICK_UPPER);
      const [upperArrayPDA] = getTickArrayPDA(program.programId, pool.publicKey, upperArrayStart);
      const { wordIndex }   = bitmapWordAndBit(arrayStartTickToBitmapIndex(upperArrayStart));
      const [bitmapPDA]     = getTickBitmapPDA(program.programId, pool.publicKey, wordIndex);

      await program.methods
        .updateTick(TICK_UPPER, LIQUIDITY, true, new BN(0), new BN(0))
        .accounts({
          tickArray:  upperArrayPDA,
          tickBitmap: bitmapPDA,
          pool:       pool.publicKey,
          authority:  wallet.publicKey,
        })
        .rpc();

      const ta   = await acct(program)["tickArray"].fetch(upperArrayPDA);
      const tick = ta.ticks[0];
      expect(tick.initialized).to.equal(true);
      const net = tick.liquidityNet as BN;
      expect(net.isNeg()).to.equal(true);
      expect(net.abs().toString()).to.equal(LIQUIDITY.toString());
      console.log(`  ✓ Upper tick ${TICK_UPPER}: initialized=true liquidityNet=-${LIQUIDITY}`);
    });
  });

  // ── On-chain: get_next_initialized_tick ───────────────────────────────────

  describe("get_next_initialized_tick", () => {
    it("finds initialized tick when searching upward", async () => {
      const [tickArrayPDA] = getTickArrayPDA(program.programId, pool.publicKey, 0);

      const result = await program.methods
        .getNextInitializedTick(-TICK_SPACING, false)
        .accounts({ tickArray: tickArrayPDA, pool: pool.publicKey })
        .view();

      expect(result).to.equal(0);
      console.log(`  ✓ Next tick above -${TICK_SPACING} = ${result}`);
    });
  });

  // ── On-chain: cross_tick ───────────────────────────────────────────────────

  describe("cross_tick", () => {
    it("returns correct liquidity_net when crossing", async () => {
      const [tickArrayPDA] = getTickArrayPDA(program.programId, pool.publicKey, 0);
      const LIQUIDITY = new BN(1_000_000);

      const result = await program.methods
        .crossTick(0, new BN(0), new BN(0))
        .accounts({
          tickArray: tickArrayPDA,
          pool:      pool.publicKey,
          authority: wallet.publicKey,
        })
        .view();

      expect(result.toString()).to.equal(LIQUIDITY.toString());
      console.log(`  ✓ cross_tick(0) liquidity_net = ${result}`);
    });
  });
});