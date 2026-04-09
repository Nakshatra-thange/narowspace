/**
 * scripts/check_position.ts
 *
 * WHAT THIS DOES:
 * Reads a live position from devnet and prints:
 *   - Tick range and current price relative to it
 *   - Liquidity amount
 *   - Estimated tokens to receive on close
 *   - Accumulated fees
 *   - Whether position is currently in-range (earning fees)
 *
 * HOW TO RUN:
 *   POSITION=<addr> POOL=<addr> npx ts-node scripts/check_position.ts
 */

import * as anchor from "@coral-xyz/anchor";
import BN from "bn.js";
import { PublicKey, Keypair, Connection } from "@solana/web3.js";
import * as fs from "fs";
import * as path from "path";

import {
  tickToPrice,
  tickToSqrtPriceQ64,
  quoteFees,
} from "../sdk/src/index.ts";

type Program = anchor.Program;
const { AnchorProvider } = anchor;

function loadWallet(): Keypair {
  const p = process.env["WALLET_PATH"]
    ?? path.join(process.env["HOME"] ?? "~", ".config", "solana", "id.json");
  return Keypair.fromSecretKey(
    Buffer.from(JSON.parse(fs.readFileSync(p, "utf-8")))
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

async function fetchDecodedAccount<T>(
  provider: anchor.AnchorProvider,
  idlPath: string,
  accountName: string,
  address: PublicKey
): Promise<T> {
  const idl = JSON.parse(fs.readFileSync(idlPath, "utf-8")) as anchor.Idl;
  const coder = new anchor.BorshCoder(idl);
  const accountInfo = await provider.connection.getAccountInfo(address);
  if (!accountInfo) {
    throw new Error(`Account not found: ${address.toString()}`);
  }
  return coder.accounts.decode(accountName, accountInfo.data) as T;
}

// Compute token amounts for a liquidity position (mirrors Rust get_amounts_for_liquidity)
function computeTokenAmountsForDisplay(
  sqrtCurrent: BN,
  sqrtLower:   BN,
  sqrtUpper:   BN,
  liquidity:   BN
): { amount0: number; amount1: number } {
  const Q64 = new BN(1).shln(64);

  function mulQ64(a: BN, b: BN): BN { return a.mul(b).shrn(64); }

  let amount0 = new BN(0);
  let amount1 = new BN(0);

  if (sqrtCurrent.lte(sqrtLower)) {
    // All token0
    const diff  = sqrtUpper.sub(sqrtLower);
    const denom = mulQ64(sqrtLower, sqrtUpper);
    amount0 = denom.isZero() ? new BN(0) : liquidity.mul(diff).div(denom);
  } else if (sqrtCurrent.gte(sqrtUpper)) {
    // All token1
    amount1 = liquidity.mul(sqrtUpper.sub(sqrtLower)).shrn(64);
  } else {
    // Split
    const diff0  = sqrtUpper.sub(sqrtCurrent);
    const denom0 = mulQ64(sqrtCurrent, sqrtUpper);
    amount0 = denom0.isZero() ? new BN(0) : liquidity.mul(diff0).div(denom0);
    amount1 = liquidity.mul(sqrtCurrent.sub(sqrtLower)).shrn(64);
  }

  return {
    amount0: amount0.toNumber(),
    amount1: amount1.toNumber(),
  };
}

async function main(): Promise<void> {
  const POSITION_ADDR = process.env["POSITION"] ?? "";
  const POOL_ADDR     = process.env["POOL"]     ?? "";
  const RPC_URL       = process.env["ANCHOR_PROVIDER_URL"] ?? "http://127.0.0.1:8899";

  if (!POSITION_ADDR || !POOL_ADDR) {
    console.error("Usage: POSITION=<addr> POOL=<addr> npx ts-node scripts/check_position.ts");
    process.exit(1);
  }

  const wallet   = loadWallet();
  const conn     = new Connection(RPC_URL, "confirmed");
  const provider = new AnchorProvider(conn, new anchor.Wallet(wallet), { commitment: "confirmed" });
  anchor.setProvider(provider);

  const poolProgId = new PublicKey("Fn65QSQyWh7w3QmAj3qhdavTd7kGEctTPwr2M8Y1253M");
  const posMgrId   = new PublicKey("4khnPYzUq44fr4WXRGrjbsLtAkK1L1A3K8ydQu1uuV8a");

  const poolProg = loadProgram(provider, poolProgId, "target/idl/pool_core.json");
  const posMgr   = loadProgram(provider, posMgrId,   "target/idl/position_mgr.json");

  const positionPubkey = new PublicKey(POSITION_ADDR);
  const poolPubkey     = new PublicKey(POOL_ADDR);

  const rawPool = await fetchDecodedAccount<{
    tick_current: number;
    fee_growth_global_0: BN;
    fee_growth_global_1: BN;
  }>(provider, "target/idl/pool_core.json", "Pool", poolPubkey);
  const rawPos = await fetchDecodedAccount<{
    owner: PublicKey;
    nft_mint: PublicKey;
    tick_lower: number;
    tick_upper: number;
    liquidity: BN;
    fee_growth_checkpoint_0: BN;
    fee_growth_checkpoint_1: BN;
    tokens_owed_0: BN;
    tokens_owed_1: BN;
  }>(provider, "target/idl/position_mgr.json", "Position", positionPubkey);

  const pool = {
    tickCurrent: rawPool.tick_current,
    feeGrowthGlobal0: rawPool.fee_growth_global_0,
    feeGrowthGlobal1: rawPool.fee_growth_global_1,
  };
  const pos = {
    owner: rawPos.owner,
    nftMint: rawPos.nft_mint,
    tickLower: rawPos.tick_lower,
    tickUpper: rawPos.tick_upper,
    liquidity: rawPos.liquidity,
    feeGrowthCheckpoint0: rawPos.fee_growth_checkpoint_0,
    feeGrowthCheckpoint1: rawPos.fee_growth_checkpoint_1,
    tokensOwed0: rawPos.tokens_owed_0,
    tokensOwed1: rawPos.tokens_owed_1,
  };

  const currentPrice = tickToPrice(pool.tickCurrent);
  const lowerPrice   = tickToPrice(pos.tickLower);
  const upperPrice   = tickToPrice(pos.tickUpper);
  const inRange      = pool.tickCurrent >= pos.tickLower
                    && pool.tickCurrent <  pos.tickUpper;

  console.log("\n══════════════════════════════════════════");
  console.log("  NarrowSwap — Position Inspector");
  console.log("══════════════════════════════════════════");
  console.log("\nPosition:", positionPubkey.toString());
  console.log("Pool:    ", poolPubkey.toString());
  console.log("Owner:   ", pos.owner.toString());
  console.log("NFT:     ", pos.nftMint.toString());
  console.log("\nPrice range:   $" + lowerPrice.toFixed(4) + " — $" + upperPrice.toFixed(4));
  console.log("Current price: $" + currentPrice.toFixed(4));
  console.log("Status:       ", inRange ? "✅ IN RANGE (earning fees)" : "⚠️  OUT OF RANGE");
  console.log("\nLiquidity:    ", pos.liquidity.toString());

  // Compute tokens to receive on close
  const sqrtCurrent = new BN(tickToSqrtPriceQ64(pool.tickCurrent).toString());
  const sqrtLower   = new BN(tickToSqrtPriceQ64(pos.tickLower).toString());
  const sqrtUpper   = new BN(tickToSqrtPriceQ64(pos.tickUpper).toString());

  const { amount0, amount1 } = computeTokenAmountsForDisplay(
    sqrtCurrent, sqrtLower, sqrtUpper, pos.liquidity
  );

  console.log("\nTokens on close (estimate):");
  console.log("  token0:", amount0.toLocaleString());
  console.log("  token1:", amount1.toLocaleString());

  // Fees owed
  const { fee0, fee1 } = quoteFees({
    positionState:    pos,
    feeGrowthGlobal0: pool.feeGrowthGlobal0,
    feeGrowthGlobal1: pool.feeGrowthGlobal1,
  });

  console.log("\nAccumulated fees:");
  console.log("  fee0:", fee0.toString(), "token0");
  console.log("  fee1:", fee1.toString(), "token1");
  console.log("\n══════════════════════════════════════════\n");
}

main().catch((err: unknown) => {
  console.error("Error:", err);
  process.exit(1);
});
