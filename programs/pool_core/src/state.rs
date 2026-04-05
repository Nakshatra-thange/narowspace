//! state.rs — Pool account definition
//!
//! The Pool account is the single source of truth for the pool's state.
//! Everything else (ticks, positions) references it by its pubkey.

use anchor_lang::prelude::*;

// ─── Pool ──────────────────────────────────────────────────────────────────────

/// The Pool account. One per trading pair.
///
/// PDA SEEDS: ["pool", token_mint_0, token_mint_1, fee_rate (as 4 bytes)]
///
/// Why include fee_rate in seeds?
/// Multiple pools can exist for the same pair with different fees.
/// A 0.05% pool for stable pairs, 0.3% for normal pairs, etc.
///
/// FIELD EXPLANATIONS:
///
/// token_mint_0 / token_mint_1:
///   The SPL token mints. By convention, mint_0 < mint_1 (sorted by pubkey).
///   This ensures there's only one canonical pool per pair.
///
/// token_vault_0 / token_vault_1:
///   The pool's ATA (associated token accounts) that hold the actual tokens.
///   When you swap, tokens move into/out of these vaults.
///   These are PDAs owned by the pool — no human can withdraw from them.
///
/// sqrt_price:
///   Current √price in Q64.64 format.
///   This is THE most important number in the pool.
///   Every swap starts here and ends with a new value here.
///
/// tick_current:
///   The tick index corresponding to the current √price.
///   Updated every time price moves through a tick boundary.
///   Used to quickly find which TickArrays are relevant for a swap.
///
/// liquidity:
///   The ACTIVE liquidity at the current price.
///   This changes every time price crosses a tick boundary (up or down).
///   Only positions whose range contains tick_current contribute to this.
///
/// fee_rate:
///   Fixed at pool creation. e.g. 3000 = 0.3%.
///
/// fee_growth_global_0 / fee_growth_global_1:
///   Global fee accumulators: total fee earned per unit of liquidity, ever.
///   These only ever increase.
///   When an LP adds liquidity, we snapshot this value.
///   When they withdraw, we compute (current - snapshot) × their_liquidity = their_fees.
///
/// protocol_fee_0 / protocol_fee_1:
///   A portion of fees reserved for the protocol (us).
///   We collect e.g. 10% of all swap fees.
///   Unused in our simplified version — tracked but not withdrawn.
///
/// initialized:
///   Safety flag. Pool can't be swapped against until this is true.
#[account]
#[derive(Default)]
pub struct Pool {
    /// Token pair
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,

    /// Token vaults (pool-owned ATAs)
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,

    /// Tick manager program — stored so we know which program to CPI into
    pub tick_manager_program: Pubkey,

    /// Current price state
    pub sqrt_price: u128,      // Q64.64
    pub tick_current: i32,
    pub _padding: [u8; 4],    // align i32 to 8 bytes

    /// Active liquidity (changes at tick crossings)
    pub liquidity: u128,

    /// Fee configuration
    pub fee_rate: u32,        // e.g. 3000 = 0.3%

    /// Fee accumulators (per unit of liquidity, ever)
    pub fee_growth_global_0: u128,
    pub fee_growth_global_1: u128,

    /// Protocol fee reserves
    pub protocol_fee_0: u64,
    pub protocol_fee_1: u64,

    /// PDA bump for this account
    pub bump: u8,

    /// Safety flag
    pub initialized: bool,
    pub _padding2: [u8; 6],
}

impl Pool {
    /// Account space:
    /// 8 (discriminator)
    /// + 32×5 (pubkeys) = 160
    /// + 16 (sqrt_price u128)
    /// + 4 (tick_current i32) + 4 (padding)
    /// + 16 (liquidity u128)
    /// + 4 (fee_rate u32)
    /// + 16+16 (fee_growth u128×2)
    /// + 8+8 (protocol_fee u64×2)
    /// + 1 (bump) + 1 (initialized) + 6 (padding)
    pub const LEN: usize = 8 + 160 + 16 + 8 + 16 + 4 + 32 + 16 + 2 + 6;
}

// ─── SwapResult — returned from swap instruction for SDK consumption ───────────

/// Everything the SDK needs to know after a swap completes.
/// Emitted as a Solana event (logged, parseable by the TS client).
#[event]
pub struct SwapEvent {
    pub pool: Pubkey,
    pub zero_for_one: bool,
    pub amount_in: u64,
    pub amount_out: u64,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub fee_amount: u64,
}