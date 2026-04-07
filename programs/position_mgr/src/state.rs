// position_mgr/src/state.rs
//
// WHAT THIS FILE CONTAINS:
// The Position account — everything needed to track one LP's liquidity range.
//
// HOW A POSITION WORKS (plain English):
// An LP deposits tokens into a price range e.g. $140-$160.
// We record: which pool, which range, how much liquidity, and a fee snapshot.
// The fee snapshot is the global fee accumulator value AT THE TIME they deposited.
// When they withdraw, we compute: (current_global_fee - snapshot) * their_liquidity
// That difference equals exactly the fees they earned while price was in their range.
//
// The NFT:
// When a position is opened, we mint one NFT to the LP's wallet.
// That NFT's mint pubkey IS the position identifier — it's stored in position.nft_mint.
// To close a position the LP must present the NFT. We burn it and return tokens + fees.
// This makes positions transferable: give someone your NFT, they own your liquidity.

use anchor_lang::prelude::*;

// ─── Position account ─────────────────────────────────────────────────────────

// PDA SEEDS: ["position", nft_mint_pubkey]
// One Position account per NFT mint. The NFT is proof of ownership.
//
// FIELD BY FIELD:
//
// pool:
//   Which pool this position belongs to.
//
// owner:
//   Who currently owns this position (holds the NFT).
//   Updated if position NFT is transferred (future feature).
//
// nft_mint:
//   The mint of the NFT receipt for this position.
//   1:1 relationship — one position = one NFT mint.
//
// tick_lower / tick_upper:
//   The price range boundaries in tick units.
//   Must be multiples of TICK_SPACING.
//   tick_lower < tick_upper always.
//
// liquidity:
//   How much liquidity this position contributes to the pool.
//   Computed from token amounts at deposit time.
//   Used by pool_core during swaps to track active liquidity.
//
// fee_growth_checkpoint_0 / fee_growth_checkpoint_1:
//   Snapshot of pool.fee_growth_global_0/1 at the time this position was last updated.
//   Fee owed = (current_global - checkpoint) * liquidity / Q64
//   These are updated every time the position is touched (add/remove liquidity).
//
// tokens_owed_0 / tokens_owed_1:
//   Accumulated but not yet collected fee tokens.
//   Increases every time we compute fees. Zeroed on collection.
//
// bump:
//   PDA bump for this account.

#[account]
#[derive(Default)]
pub struct Position {
    pub pool:                      Pubkey,
    pub owner:                     Pubkey,
    pub nft_mint:                  Pubkey,
    pub tick_lower:                i32,
    pub tick_upper:                i32,
    pub liquidity:                 u128,
    pub fee_growth_checkpoint_0:   u128,
    pub fee_growth_checkpoint_1:   u128,
    pub tokens_owed_0:             u64,
    pub tokens_owed_1:             u64,
    pub bump:                      u8,
    pub _padding:                  [u8; 7],
}

impl Position {
    // 8  discriminator
    // 32 pool
    // 32 owner
    // 32 nft_mint
    // 4  tick_lower
    // 4  tick_upper
    // 16 liquidity
    // 16 fee_growth_checkpoint_0
    // 16 fee_growth_checkpoint_1
    // 8  tokens_owed_0
    // 8  tokens_owed_1
    // 1  bump
    // 7  padding
    pub const LEN: usize = 8 + 32 + 32 + 32 + 4 + 4 + 16 + 16 + 16 + 8 + 8 + 1 + 7;

    // Compute fees owed since last checkpoint.
    // Called before any add/remove liquidity operation.
    //
    // fee_growth_inside_0/1:
    //   How much fee has accumulated per unit of liquidity INSIDE this position's range,
    //   since this position was last updated.
    //   Computed by the SDK using tick fee snapshots (see sdk/src/position.ts).
    //
    // Returns (fees_token0, fees_token1) to add to tokens_owed.
    pub fn compute_fees_owed(
        &self,
        fee_growth_inside_0: u128,
        fee_growth_inside_1: u128,
    ) -> (u64, u64) {
        // fee_owed = (fee_growth_inside - checkpoint) * liquidity / 2^64
        // wrapping_sub handles the case where accumulator wrapped around u128::MAX
        let delta_0 = fee_growth_inside_0.wrapping_sub(self.fee_growth_checkpoint_0);
        let delta_1 = fee_growth_inside_1.wrapping_sub(self.fee_growth_checkpoint_1);

        // Multiply delta (Q64 per-liquidity) by liquidity, then shift back to raw tokens
        // Result is guaranteed to fit u64 for any reasonable liquidity amount
        let fee_0 = ((delta_0 as u128).saturating_mul(self.liquidity) >> 64) as u64;
        let fee_1 = ((delta_1 as u128).saturating_mul(self.liquidity) >> 64) as u64;

        (fee_0, fee_1)
    }
}

// ─── Events ───────────────────────────────────────────────────────────────────

#[event]
pub struct PositionOpenedEvent {
    pub position:   Pubkey,
    pub pool:       Pubkey,
    pub owner:      Pubkey,
    pub nft_mint:   Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity:  u128,
    pub amount_0:   u64,
    pub amount_1:   u64,
}

#[event]
pub struct PositionClosedEvent {
    pub position:    Pubkey,
    pub pool:        Pubkey,
    pub owner:       Pubkey,
    pub liquidity:   u128,
    pub amount_0:    u64,
    pub amount_1:    u64,
    pub fees_0:      u64,
    pub fees_1:      u64,
}

#[event]
pub struct FeesCollectedEvent {
    pub position: Pubkey,
    pub owner:    Pubkey,
    pub fees_0:   u64,
    pub fees_1:   u64,
}