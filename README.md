# NarrowSwap

A concentrated liquidity AMM (Automated Market Maker) on Solana, 
built from scratch in Rust and TypeScript. Implements the core mechanics 
of Uniswap V3's pricing model — tick-based liquidity ranges, √price math, and the tick-loop swap algorithm.

**Live on Solana devnet.**

---

## What it does

NarrowSwap lets liquidity providers deposit tokens into custom price ranges instead of 
spreading liquidity across all prices. A provider who believes SOL will trade between $140–$160 can 
concentrate all their capital there — earning more fees per dollar deposited when price stays in that range.

Compared to a constant-product AMM (Uniswap V2):
- LPs earn higher fees per dollar at the active price
- Swappers get lower slippage because capital is concentrated
- LPs choose their own risk/reward by picking their range width

---

## Architecture

Three independent programs. Each has one job. They communicate via CPI (cross-program invocation) 
with zero shared code — swap out any one program without touching the others.

```
┌─────────────────────────────────────────────────────────────────┐
│                         position_mgr                            │
│  open_position / close_position / collect_fees                  │
│  Owns: Position accounts, NFT receipts                          │
│                    │ CPI: update_tick                            │
└────────────────────┼────────────────────────────────────────────┘
                     │
┌────────────────────▼────────────────────────────────────────────┐
│                         tick_manager                            │
│  initialize_tick_array / update_tick / cross_tick               │
│  Owns: TickArray accounts, TickBitmap accounts                  │
└────────────────────┬────────────────────────────────────────────┘
                     │ CPI: cross_tick (during swap)
┌────────────────────▼────────────────────────────────────────────┐
│                          pool_core                              │
│  initialize_pool / swap                                         │
│  Owns: Pool account, token vaults                               │
└─────────────────────────────────────────────────────────────────┘
```

### tick_manager
Manages the price-range index. The pool's price axis is divided into discrete slots (ticks). 
This program stores which ticks have active liquidity using two account types:

- **TickArray**: holds 88 consecutive tick slots. Each slot stores `liquidity_net`
  (how much pool liquidity changes when price crosses this tick), fee accumulators, and an initialized flag.
- **TickBitmap**: a compact bitmap where each bit represents one TickArray.
  Lets the swap engine skip empty price regions in O(1) instead of scanning every tick.

### pool_core
The swap engine. Holds pool state — current √price, current tick, active liquidity, 
fee accumulators — and executes swaps via the tick-loop algorithm.

### position_mgr
LP ownership. When a user deposits tokens into a price range, this program mints them an NFT receipt. 
The NFT represents the position — transfer it and you transfer ownership of the liquidity and accumulated fees.

---

## The tick-loop algorithm

A swap is not one calculation — it is a loop. Each iteration is one step:

1. Find the √price of the next initialized tick boundary in the swap direction
2. Compute how far price moves within this step using the swap math formulas
3. If the swap is fully consumed within this step: exit loop
4. If price hits the tick boundary first: cross the tick (apply `liquidity_net` to active liquidity),
   advance to the next tick, repeat

This loop is what makes concentrated liquidity possible. Each segment between tick boundaries has different active liquidity — the loop handles each segment independently.

```
Price →  $140        $145        $150        $155        $160
         |────────────|────────────|────────────|────────────|
LP A     └─────────────────────────────────────────────────┘  L=1000
LP B                              └────────────────────────┘  L=500

Active liquidity at each segment:
  $140–$150:  1000  (only LP A)
  $150–$160:  1500  (LP A + LP B)
```

A swap crossing $150 executes in two steps — different liquidity in each.

---

## The math

### √price storage (Q64.64)
Price is stored as √price in Q64.64 fixed-point format — a `u128` where the value 
represents `actual_√price × 2^64`. This gives 64 bits of integer precision and 64 bits of 
fractional precision without floating point.

Why √price instead of price? The liquidity formulas cancel cleanly:
- `Δtoken0 = L × (1/√P_lower − 1/√P_upper)`
- `Δtoken1 = L × (√P_upper − √P_lower)`

### Tick-to-price formula
`price = 1.0001^tick`

Tick 0 = price 1.0. Tick 69,082 ≈ price 1000. Each tick is a 0.01% price move. 
The formula is computed using precomputed magic numbers and repeated squaring — no floating point on-chain.

### Fee accounting
Global fee accumulators `fee_growth_global_0/1` track total fees earned per unit of liquidity, ever. 
When an LP opens a position, these values are snapshotted as a checkpoint. On close:

```
fees_owed = (current_global − checkpoint) × liquidity / 2^64
```

O(1) per position regardless of how many other LPs exist.

---

## Project structure

```
narrowswap/
├── programs/
│   ├── tick_manager/src/
│   │   ├── lib.rs          — 5 instructions (init array, update/cross tick, etc.)
│   │   ├── state.rs        — TickArray, TickBitmap account definitions
│   │   └── math.rs         — tick↔price conversions, Q64.64 math, bitmap helpers
│   ├── pool_core/src/
│   │   ├── lib.rs          — initialize_pool + swap (tick-loop)
│   │   ├── state.rs        — Pool account + SwapEvent
│   │   └── swap_math.rs    — compute_swap_step, Δtoken formulas, fee math
│   └── position_mgr/src/
│       ├── lib.rs          — open_position, close_position, collect_fees
│       ├── state.rs        — Position account + fee checkpoint math
│       └── liquidity_math.rs — token↔liquidity conversions
├── sdk/src/
│   ├── tick_math.ts        — TypeScript mirror of tick_manager/math.rs
│   ├── swap_math.ts        — TypeScript mirror of pool_core/swap_math.rs (quoteSwap)
│   ├── pool.ts             — initializePool, fetchPool, PDA helpers
│   ├── position.ts         — openPosition, closePosition, collectFees, quoteFees
│   └── index.ts            — barrel export
├── scripts/
│   ├── init_pool.ts        — create mints, init pool, open first LP position
│   ├── demo_swap.ts        — quote + execute a real swap on devnet
│   ├── check_position.ts   — inspect a live position's state and fees
│   └── deploy.sh           — build + deploy all programs to devnet
└── tests/
    ├── tick_manager.ts     — unit + integration tests for tick math and instructions
    ├── pool_core.ts        — pool init, swap validation tests
    └── position_mgr.ts     — full LP lifecycle: open → collect fees → close
```

---

## Devnet deployment

```bash
# Prerequisites
solana config set --url devnet
solana airdrop 4

# Build and deploy
anchor build
chmod +x scripts/deploy.sh
./scripts/deploy.sh

# Initialise a pool with liquidity
npm run init-pool

# Execute a demo swap (using addresses printed by init-pool)
POOL_ADDRESS=<addr> MINT_0=<addr> MINT_1=<addr> \
USER_ATA_0=<addr> USER_ATA_1=<addr> \
npm run demo-swap

# Inspect a position
POSITION=<addr> POOL=<addr> npm run check-position
```

---

## Running tests

```bash
# All tests against localnet (spins up automatically)
anchor test

# Individual suites
anchor test --skip-deploy -- --grep "tick_manager"
anchor test --skip-deploy -- --grep "pool_core"
anchor test --skip-deploy -- --grep "position_mgr"
```

---

## Simplified design decisions

Two intentional simplifications relative to full Uniswap V3:

**Fixed tick spacing (64):** Real pools support configurable tick spacing per fee tier. 
We use a single fixed spacing. This cuts bitmap complexity while keeping all the interesting architecture.

**Simplified fee accounting:** Full V3 tracks "fee growth inside range" by taking snapshots 
at both tick boundaries and computing the difference on each access. We use global fee 
growth as the checkpoint — accurate when price stays within the range, slightly off when it exits and re-enters. 
The architecture is identical; only the precision differs.

---

## Result 

Running NarrowSwap on a local Solana validator produced exactly the behavior the math predicts. A pool was initialized at $150 with 354,060,307 units of liquidity concentrated in the $140–$160 range. 

A swap of 1,000,000 token0 executed at an effective price of $149.29, collecting 854,384 token0 in fees at the 0.3% rate — numbers that match the off-chain quote from the TypeScript SDK to the last digit. 

The position inspector then showed the consequence of concentrated liquidity in stark terms: after the swap moved price to $2,969, the position was entirely out of range, holding 291,021,797 token1 and zero token0. Every SOL-equivalent had been sold as price rose through the range and out the other side. 

The NFT receipt, the fee accumulator, the pool liquidity counter — all updated correctly across three programs communicating only through CPI calls, with no shared state.

---

## Technical highlights

- **Three-program CPI architecture**: position_mgr calls tick_manager; pool_core reads tick_manager accounts directly during the swap loop
- **Zero-copy TickArray accounts**: `#[zero_copy]` Anchor attribute for the 88-tick array avoids heap allocation on-chain
- **PDA-owned vaults**: pool token vaults are PDAs — no private key can withdraw from them, only program logic
- **NFT position receipts**: each LP position is an SPL NFT; transfer the NFT to transfer the position
- **Off-chain quote matching on-chain execution**: the TypeScript `quoteSwap` function runs the identical tick-loop algorithm client-side so the expected output can be computed before sending a transaction
