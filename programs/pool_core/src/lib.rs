
// pool_core/src/lib.rs
//
// PROGRAM: pool_core
//
// THE TICK-LOOP EXPLAINED (plain English):
// A swap is not one calculation — it's a LOOP.
// Each iteration moves price from current toward the next tick boundary,
// consuming as much swap input as possible.
//
// If we consumed ALL remaining input -> swap done, exit loop.
// If we hit a tick boundary first -> cross the tick (liquidity changes),
// then continue loop with next step.
//
// Example (SOL/USDC pool, selling 100 SOL):
//   Current price: $150. Tick boundaries at $145, $140, $135...
//   Step 1: price moves $150->$145. Some SOL consumed. LP range ends at $145.
//   Cross tick at $145: liquidity drops.
//   Step 2: price moves $145->$140. More SOL consumed.
//   Step 3: remaining SOL consumed before $135. Loop ends.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer};

pub mod swap_math;
pub mod state;

use swap_math::*;
use state::*;

// bring in tick_manager constants and math
use tick_manager::math as tm_math;

declare_id!("HSKUQfjfpFKKyRgN1UrtS56Y1GxwCC9JjePsMoXmA2kq");

const MAX_TICK_CROSSINGS: u32 = 10;

#[program]
pub mod pool_core {
    use super::*;

    // -------------------------------------------------------------------------
    // initialize_pool
    // Creates a new pool account for a token pair.
    // initial_sqrt_price: Q64.64 sqrt price (SDK computes from human price)
    // fee_rate: e.g. 3000 = 0.3%
    // initial_tick: tick matching initial_sqrt_price
    // -------------------------------------------------------------------------
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        initial_sqrt_price: u128,
        fee_rate: u32,
        initial_tick: i32,
    ) -> Result<()> {
        require!(initial_sqrt_price > 0, PoolError::InvalidSqrtPrice);
        require!(fee_rate > 0 && fee_rate < 1_000_000, PoolError::InvalidFeeRate);
        require!(
            ctx.accounts.token_mint_0.key() < ctx.accounts.token_mint_1.key(),
            PoolError::InvalidTokenOrder
        );

        let pool = &mut ctx.accounts.pool;
        pool.token_mint_0         = ctx.accounts.token_mint_0.key();
        pool.token_mint_1         = ctx.accounts.token_mint_1.key();
        pool.token_vault_0        = ctx.accounts.token_vault_0.key();
        pool.token_vault_1        = ctx.accounts.token_vault_1.key();
        pool.tick_manager_program = ctx.accounts.tick_manager_program.key();
        pool.sqrt_price           = initial_sqrt_price;
        pool.tick_current         = initial_tick;
        pool.liquidity            = 0;
        pool.fee_rate             = fee_rate;
        pool.fee_growth_global_0  = 0;
        pool.fee_growth_global_1  = 0;
        pool.protocol_fee_0       = 0;
        pool.protocol_fee_1       = 0;
        pool.bump                 = ctx.bumps.pool;
        pool.initialized          = true;

        msg!(
            "Pool initialized: sqrt_price={} tick={} fee={}",
            initial_sqrt_price, initial_tick, fee_rate
        );
        Ok(())
    }

    // -------------------------------------------------------------------------
    // swap
    // Executes a swap via the tick-loop algorithm.
    //
    // amount:            input token amount
    // zero_for_one:      true = token0->token1 (price DOWN)
    // sqrt_price_limit:  slippage stop (below current for zero_for_one, above otherwise)
    // amount_out_minimum: revert if output is below this
    // remaining_accounts: TickArray accounts the SDK pre-computes for this swap
    // -------------------------------------------------------------------------
    pub fn swap(
        ctx: Context<Swap>,
        amount: u64,
        zero_for_one: bool,
        sqrt_price_limit: u128,
        amount_out_minimum: u64,
    ) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let pool_ai = ctx.accounts.pool.to_account_info();
        let pool = &mut ctx.accounts.pool;

        require!(pool.initialized,   PoolError::PoolNotInitialized);
        require!(amount > 0,         PoolError::ZeroAmount);
        require!(pool.liquidity > 0, PoolError::InsufficientLiquidity);

        if zero_for_one {
            require!(sqrt_price_limit < pool.sqrt_price, PoolError::InvalidPriceLimit);
        } else {
            require!(sqrt_price_limit > pool.sqrt_price, PoolError::InvalidPriceLimit);
        }

        let sqrt_price_before = pool.sqrt_price;
        let tick_before       = pool.tick_current;

        // THE TICK LOOP
        let mut sqrt_price_current = pool.sqrt_price;
        let mut tick_current       = pool.tick_current;
        let mut liquidity          = pool.liquidity;
        let mut amount_remaining   = amount as u128;
        let mut total_amount_in:  u128 = 0;
        let mut total_amount_out: u128 = 0;
        let mut total_fee:        u128 = 0;
        let mut fee_growth_inc_0: u128 = 0;
        let mut fee_growth_inc_1: u128 = 0;
        let mut crossings:        u32  = 0;

        while amount_remaining > 0 && crossings < MAX_TICK_CROSSINGS {

            // STEP 1: find sqrt price of next tick boundary
            let sqrt_price_target = find_next_sqrt_price_target(
                ctx.remaining_accounts,
                tick_current,
                zero_for_one,
                sqrt_price_limit,
            )?;

            // STEP 2: compute how far price moves and how much is consumed
            let step = compute_swap_step(
                sqrt_price_current,
                sqrt_price_target,
                liquidity,
                amount_remaining,
                pool.fee_rate as u128,
            );

            // STEP 3: update running totals
            amount_remaining = amount_remaining
                .saturating_sub(step.amount_in + step.fee_amount);
            total_amount_in  = total_amount_in.saturating_add(step.amount_in);
            total_amount_out = total_amount_out.saturating_add(step.amount_out);
            total_fee        = total_fee.saturating_add(step.fee_amount);

            // STEP 4: accumulate fee growth per unit of liquidity
            if liquidity > 0 && step.fee_amount > 0 {
                let fee_growth_delta = (step.fee_amount << 64)
                    .checked_div(liquidity)
                    .unwrap_or(0);
                if zero_for_one {
                    fee_growth_inc_0 = fee_growth_inc_0.saturating_add(fee_growth_delta);
                } else {
                    fee_growth_inc_1 = fee_growth_inc_1.saturating_add(fee_growth_delta);
                }
            }

            sqrt_price_current = step.sqrt_price_next;

            // STEP 5: cross the tick if we hit the boundary
            if step.sqrt_price_next == sqrt_price_target {
                let liquidity_net = read_liquidity_net_at_tick(
                    ctx.remaining_accounts,
                    sqrt_price_target,
                    zero_for_one,
                )?;

                liquidity = apply_liquidity_delta(liquidity, liquidity_net);

                tick_current = if zero_for_one {
                    tick_at_sqrt_price(sqrt_price_target).saturating_sub(1)
                } else {
                    tick_at_sqrt_price(sqrt_price_target)
                };

                crossings += 1;

                if zero_for_one && sqrt_price_current <= sqrt_price_limit { break; }
                if !zero_for_one && sqrt_price_current >= sqrt_price_limit { break; }
            } else {
                // Swap fully consumed
                tick_current = tick_at_sqrt_price(sqrt_price_current);
                break;
            }
        }

        // VALIDATE OUTPUT
        require!(total_amount_out > 0, PoolError::ZeroOutput);
        require!(
            total_amount_out >= amount_out_minimum as u128,
            PoolError::SlippageExceeded
        );

        // WRITE NEW POOL STATE
        pool.sqrt_price          = sqrt_price_current;
        pool.tick_current        = tick_current;
        pool.liquidity           = liquidity;
        pool.fee_growth_global_0 = pool.fee_growth_global_0.saturating_add(fee_growth_inc_0);
        pool.fee_growth_global_1 = pool.fee_growth_global_1.saturating_add(fee_growth_inc_1);

        let protocol_share = total_fee / 10;
        if zero_for_one {
            pool.protocol_fee_0 = pool.protocol_fee_0.saturating_add(protocol_share as u64);
        } else {
            pool.protocol_fee_1 = pool.protocol_fee_1.saturating_add(protocol_share as u64);
        }

        // TOKEN TRANSFERS
        let amount_in_u64  = u64::try_from(total_amount_in)
            .map_err(|_| error!(PoolError::AmountOverflow))?;
        let amount_out_u64 = u64::try_from(total_amount_out)
            .map_err(|_| error!(PoolError::AmountOverflow))?;

        let mint_0_key     = pool.token_mint_0;
        let mint_1_key     = pool.token_mint_1;
        let fee_rate_bytes = pool.fee_rate.to_le_bytes();
        let bump_val       = pool.bump;
        let pool_seeds     = &[
            b"pool".as_ref(),
            mint_0_key.as_ref(),
            mint_1_key.as_ref(),
            fee_rate_bytes.as_ref(),
            &[bump_val],
        ];
        let signer_seeds = &[&pool_seeds[..]];

        if zero_for_one {
            // User sends token0 IN
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.user_token_account_0.to_account_info(),
                        to:        ctx.accounts.token_vault_0.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_in_u64,
            )?;
            // Pool sends token1 OUT
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.token_vault_1.to_account_info(),
                        to:        ctx.accounts.user_token_account_1.to_account_info(),
                        authority: pool_ai.clone()
                    },
                    signer_seeds,
                ),
                amount_out_u64,
            )?;
        } else {
            // User sends token1 IN
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.user_token_account_1.to_account_info(),
                        to:        ctx.accounts.token_vault_1.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_in_u64,
            )?;
            // Pool sends token0 OUT
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.token_vault_0.to_account_info(),
                        to:        ctx.accounts.user_token_account_0.to_account_info(),
                        authority: pool_ai.clone(),
                    },
                    signer_seeds,
                ),
                amount_out_u64,
            )?;
        }

        emit!(SwapEvent {
            pool:             pool_key,
            zero_for_one,
            amount_in:        amount_in_u64,
            amount_out:       amount_out_u64,
            sqrt_price_before,
            sqrt_price_after:  pool.sqrt_price,
            tick_before,
            tick_after:       pool.tick_current,
            fee_amount:       u64::try_from(total_fee).unwrap_or(u64::MAX),
        });

        msg!(
            "Swap done: in={} out={} fee={} crossings={}",
            amount_in_u64, amount_out_u64, total_fee, crossings
        );

        Ok(())
    }
}

// ─── Tick-loop helpers ─────────────────────────────────────────────────────────

// TickArray raw account layout offsets:
// 8 bytes  discriminator
// 4 bytes  start_tick_index (i32)
// 4 bytes  padding
// 32 bytes pool pubkey
// = 48 bytes header, then ticks begin
const HEADER_SIZE: usize = 8 + 4 + 4 + 32;

// TickData layout (64 bytes per tick):
// 0..16   liquidity_net  (i128)
// 16..32  liquidity_gross (u128)
// 32..48  fee_growth_outside_0 (u128)
// 48..64  fee_growth_outside_1 (u128)
// 64      initialized (bool, 1 byte)
// 65..80  padding (15 bytes)
const TICK_DATA_SIZE: usize = 64;
const INITIALIZED_OFFSET: usize = 16 + 16 + 16 + 16; // = 64, byte within TickData

/// Scan remaining_accounts to find the sqrt price of the next initialized tick
/// in the swap direction. Falls back to price_limit if no tick found.
fn find_next_sqrt_price_target<'info>(
    remaining_accounts: &[AccountInfo<'info>],
    current_tick: i32,
    zero_for_one: bool,
    price_limit: u128,
) -> Result<u128> {
    let tick_spacing  = tm_math::TICK_SPACING;
    let ticks_per_arr = tm_math::TICKS_PER_ARRAY as i32;
    let array_size    = ticks_per_arr * tick_spacing;

    let mut best_tick: Option<i32> = None;

    for acct in remaining_accounts.iter() {
        let data = acct.try_borrow_data()?;
        if data.len() < HEADER_SIZE { continue; }

        let start_tick = i32::from_le_bytes(
            data[8..12].try_into().map_err(|_| error!(PoolError::InvalidTickArray))?
        );

        if zero_for_one {
            if start_tick >= current_tick { continue; }
            let max_slot = (((current_tick - start_tick) / tick_spacing) as usize)
                .min(tm_math::TICKS_PER_ARRAY - 1);
            for slot in (0..=max_slot).rev() {
                let off = HEADER_SIZE + slot * TICK_DATA_SIZE + INITIALIZED_OFFSET;
                if off >= data.len() { continue; }
                if data[off] != 0 {
                    let t = start_tick + (slot as i32) * tick_spacing;
                    best_tick = Some(best_tick.map_or(t, |p: i32| p.max(t)));
                }
            }
        } else {
            if start_tick + array_size <= current_tick { continue; }
            let start_slot = (((current_tick - start_tick) / tick_spacing + 1).max(0)) as usize;
            for slot in start_slot..tm_math::TICKS_PER_ARRAY {
                let off = HEADER_SIZE + slot * TICK_DATA_SIZE + INITIALIZED_OFFSET;
                if off >= data.len() { continue; }
                if data[off] != 0 {
                    let t = start_tick + (slot as i32) * tick_spacing;
                    best_tick = Some(best_tick.map_or(t, |p: i32| p.min(t)));
                }
            }
        }
    }

    match best_tick {
        Some(t) => tm_math::tick_to_sqrt_price_q64(t)
            .map_err(|_| error!(PoolError::InvalidTickArray)),
        None => Ok(price_limit),
    }
}

/// Read liquidity_net from the TickArray account that contains sqrt_price_target.
fn read_liquidity_net_at_tick<'info>(
    remaining_accounts: &[AccountInfo<'info>],
    sqrt_price_target: u128,
    zero_for_one: bool,
) -> Result<i128> {
    let target_tick  = tick_at_sqrt_price(sqrt_price_target);
    let tick_spacing = tm_math::TICK_SPACING;
    let array_size   = tm_math::TICKS_PER_ARRAY as i32 * tick_spacing;

    for acct in remaining_accounts.iter() {
        let data = acct.try_borrow_data()?;
        if data.len() < HEADER_SIZE { continue; }

        let start_tick = i32::from_le_bytes(
            data[8..12].try_into().map_err(|_| error!(PoolError::InvalidTickArray))?
        );

        if target_tick < start_tick || target_tick >= start_tick + array_size { continue; }

        let slot   = ((target_tick - start_tick) / tick_spacing) as usize;
        let offset = HEADER_SIZE + slot * TICK_DATA_SIZE;
        if offset + 16 > data.len() { return err!(PoolError::InvalidTickArray); }

        let bytes: [u8; 16] = data[offset..offset + 16]
            .try_into()
            .map_err(|_| error!(PoolError::InvalidTickArray))?;
        let net = i128::from_le_bytes(bytes);

        // Convention: we already negated for zero_for_one in the caller; just return.
        // Actually: negate here — when crossing downward the net sign flips.
        return Ok(if zero_for_one { -net } else { net });
    }
    Ok(0) // no tick found — no liquidity change
}

/// Apply signed liquidity_net to current pool liquidity.
fn apply_liquidity_delta(liquidity: u128, liquidity_net: i128) -> u128 {
    if liquidity_net >= 0 {
        liquidity.saturating_add(liquidity_net as u128)
    } else {
        liquidity.saturating_sub((-liquidity_net) as u128)
    }
}

/// Approximate tick from Q64.64 sqrt price. Used for state tracking only.
fn tick_at_sqrt_price(sqrt_price_q64: u128) -> i32 {
    let int_part = (sqrt_price_q64 >> 64) as u64;
    if int_part == 0 { return tm_math::MIN_TICK; }
    let log2 = 63 - int_part.leading_zeros();
    let tick  = (log2 as i32) * 2 * 13328;
    tick.clamp(tm_math::MIN_TICK, tm_math::MAX_TICK)
}

// ─── Account structs ──────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(initial_sqrt_price: u128, fee_rate: u32)]
pub struct InitializePool<'info> {
    #[account(
        init,
        payer = payer,
        space = Pool::LEN,
        seeds = [
            b"pool",
            token_mint_0.key().as_ref(),
            token_mint_1.key().as_ref(),
            &fee_rate.to_le_bytes(),
        ],
        bump
    )]
    pub pool: Account<'info, Pool>,

    pub token_mint_0: Account<'info, Mint>,
    pub token_mint_1: Account<'info, Mint>,

    #[account(
        init,
        payer = payer,
        token::mint = token_mint_0,
        token::authority = pool,
        seeds = [b"vault_0", pool.key().as_ref()],
        bump
    )]
    pub token_vault_0: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = payer,
        token::mint = token_mint_1,
        token::authority = pool,
        seeds = [b"vault_1", pool.key().as_ref()],
        bump
    )]
    pub token_vault_1: Account<'info, TokenAccount>,

    /// CHECK: stored as reference only — not validated beyond being a valid pubkey
    pub tick_manager_program: UncheckedAccount<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub token_program:  Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent:           Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(
        mut,
        seeds = [
            b"pool",
            pool.token_mint_0.as_ref(),
            pool.token_mint_1.as_ref(),
            &pool.fee_rate.to_le_bytes(),
        ],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"vault_0", pool.key().as_ref()],
        bump
    )]
    pub token_vault_0: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"vault_1", pool.key().as_ref()],
        bump
    )]
    pub token_vault_1: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_token_account_0.mint  == pool.token_mint_0 @ PoolError::InvalidTokenAccount,
        constraint = user_token_account_0.owner == user.key()        @ PoolError::InvalidTokenAccount,
    )]
    pub user_token_account_0: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_token_account_1.mint  == pool.token_mint_1 @ PoolError::InvalidTokenAccount,
        constraint = user_token_account_1.owner == user.key()        @ PoolError::InvalidTokenAccount,
    )]
    pub user_token_account_1: Account<'info, TokenAccount>,

    pub user:          Signer<'info>,
    pub token_program: Program<'info, Token>,
    // remaining_accounts = TickArray accounts SDK pre-computes
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[error_code]
pub enum PoolError {
    #[msg("Initial sqrt price must be positive")]
    InvalidSqrtPrice,
    #[msg("Fee rate must be between 1 and 999999")]
    InvalidFeeRate,
    #[msg("token_mint_0 pubkey must be less than token_mint_1")]
    InvalidTokenOrder,
    #[msg("Pool is not initialized")]
    PoolNotInitialized,
    #[msg("Swap amount must be greater than zero")]
    ZeroAmount,
    #[msg("Pool has no active liquidity")]
    InsufficientLiquidity,
    #[msg("Price limit is in wrong direction for this swap")]
    InvalidPriceLimit,
    #[msg("Swap produced zero output")]
    ZeroOutput,
    #[msg("Output below minimum: slippage exceeded")]
    SlippageExceeded,
    #[msg("Token account mint or owner mismatch")]
    InvalidTokenAccount,
    #[msg("Could not parse tick array from remaining_accounts")]
    InvalidTickArray,
    #[msg("Token amount overflows u64")]
    AmountOverflow,
}
