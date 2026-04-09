/**
 * scripts/demo_swap.ts
 *
 * WHAT THIS DOES:
 * Executes a real swap against the pool initialised by init_pool.ts.
 * Demonstrates the full swap flow:
 *   1. Quote the swap off-chain (no transaction yet)
 *   2. Show expected output and price impact
 *   3. Execute the on-chain swap with slippage protection
 *   4. Print before/after token balances
 *
 * HOW TO RUN (after init_pool.ts):
 *   npx ts-node scripts/demo_swap.ts \
 *     --pool  <POOL_ADDRESS>  \
 *     --mint0 <MINT_0>        \
 *     --mint1 <MINT_1>        \
 *     --ata0  <USER_ATA_0>    \
 *     --ata1  <USER_ATA_1>
 *
 * OR: edit the ADDRESSES section below directly.
 */

import * as anchor from "@coral-xyz/anchor";
import BN from "bn.js";
import {
  PublicKey,
  Keypair,
  Connection,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  getAccount,
} from "@solana/spl-token";
import * as fs from "fs";
import * as path from "path";

import {
  tickToPrice,
  getVaultPDA,
  getTickArrayPDA,
  tickToArrayStartTick,
  quoteSwap,
} from "../sdk/src/index.ts";

type Program = anchor.Program;
const { AnchorProvider } = anchor;

// ─── ADDRESSES (fill from init_pool.ts output) ────────────────────────────────

// These defaults are placeholders. Either pass CLI args or edit here.
let POOL_ADDRESS = process.env["POOL_ADDRESS"] ?? "";
let MINT_0       = process.env["MINT_0"]       ?? "";
let MINT_1       = process.env["MINT_1"]       ?? "";
let USER_ATA_0   = process.env["USER_ATA_0"]   ?? "";
let USER_ATA_1   = process.env["USER_ATA_1"]   ?? "";
const RPC_URL     = process.env["ANCHOR_PROVIDER_URL"] ?? "http://127.0.0.1:8899";
const TM_PROGRAM_ID   = "9V4BX9p6bRy37gWMDR5xatdntPQKdzU6DXpf3gqsBumW";
const POOL_PROGRAM_ID = "Fn65QSQyWh7w3QmAj3qhdavTd7kGEctTPwr2M8Y1253M";

// Parse CLI args: --pool <addr> --mint0 <addr> etc.
const args = process.argv.slice(2);
for (let i = 0; i < args.length - 1; i++) {
  const flag = args[i];
  const val  = args[i + 1] ?? "";
  if (flag === "--pool")  POOL_ADDRESS = val;
  if (flag === "--mint0") MINT_0       = val;
  if (flag === "--mint1") MINT_1       = val;
  if (flag === "--ata0")  USER_ATA_0   = val;
  if (flag === "--ata1")  USER_ATA_1   = val;
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

function loadWallet(): Keypair {
  const walletPath = process.env["WALLET_PATH"]
    ?? path.join(process.env["HOME"] ?? "~", ".config", "solana", "id.json");
  return Keypair.fromSecretKey(
    Buffer.from(JSON.parse(fs.readFileSync(walletPath, "utf-8")))
  );
}

function loadProgram(
  provider: anchor.AnchorProvider,
  programId: PublicKey,
  idlPath: string
): Program {
  const idl = {
    ...(JSON.parse(fs.readFileSync(idlPath, "utf-8")) as anchor.Idl),
    address: programId.toString(),
  };
  return new anchor.Program(idl, provider);
}

async function fetchPoolState(
  provider: anchor.AnchorProvider,
  address: PublicKey
): Promise<{
  sqrtPrice: BN;
  tickCurrent: number;
  liquidity: BN;
  feeRate: number;
}> {
  const idl = JSON.parse(fs.readFileSync("target/idl/pool_core.json", "utf-8")) as anchor.Idl;
  const coder = new anchor.BorshCoder(idl);
  const accountInfo = await provider.connection.getAccountInfo(address);
  if (!accountInfo) {
    throw new Error(`Pool account not found: ${address.toString()}`);
  }
  const raw = coder.accounts.decode("Pool", accountInfo.data) as {
    sqrt_price: BN;
    tick_current: number;
    liquidity: BN;
    fee_rate: number;
  };
  return {
    sqrtPrice: raw.sqrt_price,
    tickCurrent: raw.tick_current,
    liquidity: raw.liquidity,
    feeRate: raw.fee_rate,
  };
}

// ─── Main ─────────────────────────────────────────────────────────────────────

async function main(): Promise<void> {
  // Validate required addresses
  if (!POOL_ADDRESS || !MINT_0 || !MINT_1 || !USER_ATA_0 || !USER_ATA_1) {
    console.error(
      "Missing required addresses.\n" +
      "Run init_pool.ts first, then pass addresses via env or CLI args:\n" +
      "  POOL_ADDRESS=<addr> MINT_0=<addr> ... npx ts-node scripts/demo_swap.ts\n" +
      "  OR: npx ts-node scripts/demo_swap.ts --pool <addr> --mint0 <addr> ..."
    );
    process.exit(1);
  }

  const wallet   = loadWallet();
  const conn     = new Connection(RPC_URL, "confirmed");
  const provider = new AnchorProvider(conn, new anchor.Wallet(wallet), { commitment: "confirmed" });
  anchor.setProvider(provider);

  const tmProgId   = new PublicKey(TM_PROGRAM_ID);
  const poolProgId = new PublicKey(POOL_PROGRAM_ID);

  const tmProg   = loadProgram(provider, tmProgId,   "target/idl/tick_manager.json");
  const poolProg = loadProgram(provider, poolProgId, "target/idl/pool_core.json");

  const poolPubkey = new PublicKey(POOL_ADDRESS);
  const userAta0   = new PublicKey(USER_ATA_0);
  const userAta1   = new PublicKey(USER_ATA_1);

  const [vault0] = getVaultPDA(poolProgId, poolPubkey, 0);
  const [vault1] = getVaultPDA(poolProgId, poolPubkey, 1);

  // ── Fetch current pool state ─────────────────────────────────────────────────

  console.log("\n══════════════════════════════════════════");
  console.log("  NarrowSwap — Demo Swap");
  console.log("══════════════════════════════════════════");

  const pool = await fetchPoolState(provider, poolPubkey);
  const currentPrice = tickToPrice(pool.tickCurrent);

  console.log("\nPool state:");
  console.log("  Address:   ", poolPubkey.toString());
  console.log("  Tick:      ", pool.tickCurrent);
  console.log("  Price:     ", currentPrice.toFixed(4), "(token1 per token0)");
  console.log("  Liquidity: ", pool.liquidity.toString());
  console.log("  Fee rate:  ", pool.feeRate / 10_000, "%");

  if (pool.liquidity.isZero()) {
    console.error("\nPool has no liquidity. Run init_pool.ts first.");
    process.exit(1);
  }

  // ── Token balances before ────────────────────────────────────────────────────

  const ata0Before = await getAccount(conn, userAta0);
  const ata1Before = await getAccount(conn, userAta1);

  console.log("\nBalances before swap:");
  console.log("  token0:", Number(ata0Before.amount).toLocaleString());
  console.log("  token1:", Number(ata1Before.amount).toLocaleString());

  // ── Quote the swap off-chain ──────────────────────────────────────────────────
  // Swap 100 token0 for token1 (zero_for_one = true, price goes down)

  const SWAP_AMOUNT_0 = new BN(100_000); // 0.1 token0
  const ZERO_FOR_ONE  = true;

  // Find tick arrays for this swap (arrays below current tick)
  const arraysForSwap: PublicKey[] = [];
  for (let i = 0; i <= 2; i++) {
    const startTick = tickToArrayStartTick(pool.tickCurrent - (i * 88 * 64));
    const [arrPDA]  = getTickArrayPDA(tmProgId, poolPubkey, startTick);
    arraysForSwap.push(arrPDA);
  }

  // Keep the swap close to the current price to avoid large demo moves.
  const sqrtPriceLimit = pool.sqrtPrice.subn(1_000);

  let amountOutMin = new BN(0);
  try {
    const quote = quoteSwap({
      sqrtPriceCurrent: pool.sqrtPrice,
      liquidityCurrent: pool.liquidity,
      tickCurrent:      pool.tickCurrent,
      feeRate:          new BN(pool.feeRate),
      zeroForOne:       ZERO_FOR_ONE,
      amount:           SWAP_AMOUNT_0,
      sqrtPriceLimit,
      initializedTicks: [],
    });

    console.log("\nSwap quote (off-chain):");
    console.log("  Input:        ", SWAP_AMOUNT_0.toString(), "token0");
    console.log("  Expected out: ", quote.amountOut.toString(), "token1");
    console.log("  Fee:          ", quote.feeAmount.toString(), "token0");
    console.log("  Price impact: ", quote.priceImpactBps, "bps");
    console.log("  Tick crossings:", quote.tickCrossings);

    if (quote.amountOut.gtn(0) && !quote.amountOut.isNeg()) {
      amountOutMin = quote.amountOut.muln(9950).divn(10000);
    }
  } catch (error) {
    console.log("\nSwap quote (off-chain) skipped:", error instanceof Error ? error.message : String(error));
  }

  console.log("  Min output:   ", amountOutMin.toString(), "(demo slippage floor)");

  // ── Execute the swap ─────────────────────────────────────────────────────────

  console.log("\nExecuting swap on-chain...");

  const swapTx = await poolProg.methods
    .swap(
      SWAP_AMOUNT_0,
      ZERO_FOR_ONE,
      sqrtPriceLimit,
      amountOutMin
    )
    .accounts({
      pool:                poolPubkey,
      tokenVault0:         vault0,
      tokenVault1:         vault1,
      userTokenAccount0:   userAta0,
      userTokenAccount1:   userAta1,
      user:                wallet.publicKey,
      tokenProgram:        TOKEN_PROGRAM_ID,
    })
    .remainingAccounts(
      arraysForSwap.map((pk) => ({
        pubkey:     pk,
        isWritable: false,
        isSigner:   false,
      }))
    )
    .rpc();

  console.log("  Tx:", swapTx);
  console.log("  Signature:", swapTx);

  // ── Balances after ────────────────────────────────────────────────────────────

  const ata0After  = await getAccount(conn, userAta0);
  const ata1After  = await getAccount(conn, userAta1);
  const poolAfter  = await fetchPoolState(provider, poolPubkey);

  const token0Spent    = Number(ata0Before.amount) - Number(ata0After.amount);
  const token1Received = Number(ata1After.amount)  - Number(ata1Before.amount);

  console.log("\nBalances after swap:");
  console.log("  token0:", Number(ata0After.amount).toLocaleString(),
    "(-" + token0Spent.toLocaleString() + ")"
  );
  console.log("  token1:", Number(ata1After.amount).toLocaleString(),
    "(+" + token1Received.toLocaleString() + ")"
  );

  console.log("\nPool state after:");
  console.log("  Tick:      ", poolAfter.tickCurrent,
    "(was " + pool.tickCurrent + ")"
  );
  console.log("  Price:     ", tickToPrice(poolAfter.tickCurrent).toFixed(4));
  console.log("  Liquidity: ", poolAfter.liquidity.toString());

  const effectivePrice = token1Received / token0Spent;
  console.log("\nEffective price:", effectivePrice.toFixed(6), "token1/token0");

  console.log("\n══════════════════════════════════════════");
  console.log("  Swap complete");
  console.log("══════════════════════════════════════════\n");
}

main().catch((err: unknown) => {
  console.error("Error:", err);
  process.exit(1);
});
