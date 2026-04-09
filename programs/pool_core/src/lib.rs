// pool_core/src/lib.rs

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer};

pub mod swap_math;
pub mod state;

use swap_math::*;
use state::*;
use tick_manager::math as tm_math;

declare_id!("Fn65QSQyWh7w3QmAj3qhdavTd7kGEctTPwr2M8Y1253M");

const MAX_TICK_CROSSINGS: u32 = 10;

#[program]
pub mod pool_core {
    use super::*;

    // -------------------------------------------------------------------------
    // initialize_pool
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

        msg!("Pool initialized: sqrt_price={} tick={} fee={}", initial_sqrt_price, initial_tick, fee_rate);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // modify_liquidity
    //
    // Called by position_mgr via CPI when an LP opens or closes a position.
    // Updates pool.liquidity by delta (positive = add, negative = remove).
    // Only changes liquidity if tick_current is inside [tick_lower, tick_upper).
    //
    // WHY THIS IS IN pool_core:
    // pool_core owns the Pool account. Only pool_core can write to it.
    // position_mgr cannot directly modify pool.liquidity — it must CPI here.
    // -------------------------------------------------------------------------
    pub fn modify_liquidity(
        ctx: Context<ModifyLiquidity>,
        liquidity_delta: i128,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        require!(pool.initialized, PoolError::PoolNotInitialized);

        let in_range = pool.tick_current >= tick_lower
            && pool.tick_current < tick_upper;

        if in_range {
            if liquidity_delta >= 0 {
                pool.liquidity = pool.liquidity.saturating_add(liquidity_delta as u128);
            } else {
                pool.liquidity = pool.liquidity.saturating_sub((-liquidity_delta) as u128);
            }
        }

        msg!("modify_liquidity: delta={} in_range={} new_liquidity={}", liquidity_delta, in_range, pool.liquidity);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // transfer_from_vault
    //
    // Called by position_mgr via CPI to send tokens from pool vaults to a user.
    // The pool PDA signs for the vault transfer — only pool_core can do this.
    //
    // vault_index: 0 = vault0 (token0), 1 = vault1 (token1)
    // amount: how many tokens to send
    // -------------------------------------------------------------------------
    pub fn transfer_from_vault(
        ctx: Context<TransferFromVault>,
        vault_index: u8,
        amount: u64,
    ) -> Result<()> {
        if amount == 0 { return Ok(()); }

        let pool = &ctx.accounts.pool;

        let mint_0_key    = pool.token_mint_0;
        let mint_1_key    = pool.token_mint_1;
        let fee_rate_bytes = pool.fee_rate.to_le_bytes();
        let bump          = pool.bump;

        let pool_seeds = &[
            b"pool".as_ref(),
            mint_0_key.as_ref(),
            mint_1_key.as_ref(),
            fee_rate_bytes.as_ref(),
            &[bump],
        ];
        let signer_seeds = &[&pool_seeds[..]];

        if vault_index == 0 {
            require!(
                ctx.accounts.vault.mint == pool.token_mint_0,
                PoolError::InvalidTokenAccount
            );
        } else {
            require!(
                ctx.accounts.vault.mint == pool.token_mint_1,
                PoolError::InvalidTokenAccount
            );
        }

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from:      ctx.accounts.vault.to_account_info(),
                    to:        ctx.accounts.destination.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                signer_seeds,
            ),
            amount,
        )?;

        msg!("transfer_from_vault: vault_index={} amount={}", vault_index, amount);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // swap
    // -------------------------------------------------------------------------
    pub fn swap(
        ctx: Context<Swap>,
        amount: u64,
        zero_for_one: bool,
        sqrt_price_limit: u128,
        amount_out_minimum: u64,
    ) -> Result<()> {
        let pool_account_info = ctx.accounts.pool.to_account_info();
        let pool_key = ctx.accounts.pool.key();
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
            let sqrt_price_target = find_next_sqrt_price_target(
                ctx.remaining_accounts,
                tick_current,
                zero_for_one,
                sqrt_price_limit,
            )?;

            let step = compute_swap_step(
                sqrt_price_current,
                sqrt_price_target,
                liquidity,
                amount_remaining,
                pool.fee_rate as u128,
            );

            amount_remaining = amount_remaining.saturating_sub(step.amount_in + step.fee_amount);
            total_amount_in  = total_amount_in.saturating_add(step.amount_in);
            total_amount_out = total_amount_out.saturating_add(step.amount_out);
            total_fee        = total_fee.saturating_add(step.fee_amount);

            if liquidity > 0 && step.fee_amount > 0 {
                let fee_growth_delta = (step.fee_amount << 64).checked_div(liquidity).unwrap_or(0);
                if zero_for_one {
                    fee_growth_inc_0 = fee_growth_inc_0.saturating_add(fee_growth_delta);
                } else {
                    fee_growth_inc_1 = fee_growth_inc_1.saturating_add(fee_growth_delta);
                }
            }

            sqrt_price_current = step.sqrt_price_next;

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
                tick_current = tick_at_sqrt_price(sqrt_price_current);
                break;
            }
        }

        require!(total_amount_out > 0, PoolError::ZeroOutput);
        require!(total_amount_out >= amount_out_minimum as u128, PoolError::SlippageExceeded);

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

        let amount_in_u64  = u64::try_from(total_amount_in).map_err(|_| error!(PoolError::AmountOverflow))?;
        let amount_out_u64 = u64::try_from(total_amount_out).map_err(|_| error!(PoolError::AmountOverflow))?;

        let mint_0_key    = pool.token_mint_0;
        let mint_1_key    = pool.token_mint_1;
        let fee_rate_bytes = pool.fee_rate.to_le_bytes();
        let bump_val      = pool.bump;
        let pool_seeds    = &[
            b"pool".as_ref(),
            mint_0_key.as_ref(),
            mint_1_key.as_ref(),
            fee_rate_bytes.as_ref(),
            &[bump_val],
        ];
        let signer_seeds = &[&pool_seeds[..]];

        if zero_for_one {
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
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.token_vault_1.to_account_info(),
                        to:        ctx.accounts.user_token_account_1.to_account_info(),
                        authority: pool_account_info.clone(),
                    },
                    signer_seeds,
                ),
                amount_out_u64,
            )?;
        } else {
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
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from:      ctx.accounts.token_vault_0.to_account_info(),
                        to:        ctx.accounts.user_token_account_0.to_account_info(),
                        authority: pool_account_info.clone(),
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

        msg!("Swap done: in={} out={} fee={} crossings={}", amount_in_u64, amount_out_u64, total_fee, crossings);
        Ok(())
    }
}

// ─── Tick-loop helpers ────────────────────────────────────────────────────────

const HEADER_SIZE:        usize = 8 + 4 + 4 + 32;
const TICK_DATA_SIZE:     usize = 80;
const INITIALIZED_OFFSET: usize = 16 + 16 + 16 + 16;

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
        return Ok(if zero_for_one { -net } else { net });
    }
    Ok(0)
}

fn apply_liquidity_delta(liquidity: u128, liquidity_net: i128) -> u128 {
    if liquidity_net >= 0 {
        liquidity.saturating_add(liquidity_net as u128)
    } else {
        liquidity.saturating_sub((-liquidity_net) as u128)
    }
}

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

    /// CHECK: stored as reference only
    pub tick_manager_program: UncheckedAccount<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub token_program:  Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent:           Sysvar<'info, Rent>,
}

// modify_liquidity — called by position_mgr CPI
#[derive(Accounts)]
pub struct ModifyLiquidity<'info> {
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

    // No authority check here — position_mgr is the only caller and
    // it has no privileged key. We trust the program logic, not a key.
    // In production you'd add a position_mgr_program_id check.
}

// transfer_from_vault — called by position_mgr CPI
#[derive(Accounts)]
pub struct TransferFromVault<'info> {
    #[account(
        seeds = [
            b"pool",
            pool.token_mint_0.as_ref(),
            pool.token_mint_1.as_ref(),
            &pool.fee_rate.to_le_bytes(),
        ],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub destination: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
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
