// position_mgr/src/lib.rs
//
// All pool state mutations and vault transfers go through pool_core CPI.
// position_mgr never writes Pool directly — pool_core owns it.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, MintTo, Burn, Transfer};
use anchor_spl::associated_token::AssociatedToken;

pub mod state;
pub mod liquidity_math;

use state::*;
use liquidity_math::*;
use tick_manager::math as tm_math;

declare_id!("4khnPYzUq44fr4WXRGrjbsLtAkK1L1A3K8ydQu1uuV8a");

fn bitmap_word_index_for_tick(tick_index: i32) -> i32 {
    let array_start = tm_math::tick_to_array_start_tick(tick_index);
    let array_index = tm_math::array_start_to_bitmap_index(array_start);
    let (word_index, _) = tm_math::bitmap_word_and_bit(array_index);
    word_index
}

#[program]
pub mod position_mgr {
    use super::*;

    // -------------------------------------------------------------------------
    // open_position
    // -------------------------------------------------------------------------
    pub fn open_position(
        ctx: Context<OpenPosition>,
        amount_0_desired: u64,
        amount_1_desired: u64,
        amount_0_minimum: u64,
        amount_1_minimum: u64,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<()> {
        require!(tick_lower < tick_upper,                  PositionError::InvalidTickRange);
        require!(tick_lower % tm_math::TICK_SPACING == 0, PositionError::InvalidTickRange);
        require!(tick_upper % tm_math::TICK_SPACING == 0, PositionError::InvalidTickRange);
        require!(tick_lower >= tm_math::MIN_TICK,          PositionError::InvalidTickRange);
        require!(tick_upper <= tm_math::MAX_TICK,          PositionError::InvalidTickRange);

        let pool_state = &ctx.accounts.pool;
        require!(pool_state.initialized, PositionError::PoolNotInitialized);
        let lower_bitmap_word_index = bitmap_word_index_for_tick(tick_lower);
        let upper_bitmap_word_index = bitmap_word_index_for_tick(tick_upper);

        let sqrt_price_current = pool_state.sqrt_price;
        let sqrt_price_lower = tm_math::tick_to_sqrt_price_q64(tick_lower)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;
        let sqrt_price_upper = tm_math::tick_to_sqrt_price_q64(tick_upper)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;

        let liquidity = get_liquidity_for_amounts(
            sqrt_price_current, sqrt_price_lower, sqrt_price_upper,
            amount_0_desired, amount_1_desired,
        );
        require!(liquidity > 0, PositionError::ZeroLiquidity);

        let (amount_0, amount_1) = get_amounts_for_liquidity(
            sqrt_price_current, sqrt_price_lower, sqrt_price_upper, liquidity,
        );
        require!(amount_0 >= amount_0_minimum, PositionError::SlippageExceeded);
        require!(amount_1 >= amount_1_minimum, PositionError::SlippageExceeded);

        // CPI: register lower tick
        tick_manager::cpi::update_tick(
            CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array:  ctx.accounts.tick_array_lower.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_lower.to_account_info(),
                    pool:        ctx.accounts.pool.to_account_info(),
                    authority:   ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            ),
            tick_lower, lower_bitmap_word_index, liquidity as i128, false,
            pool_state.fee_growth_global_0, pool_state.fee_growth_global_1,
        )?;

        // CPI: register upper tick
        tick_manager::cpi::update_tick(
            CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array:  ctx.accounts.tick_array_upper.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_upper.to_account_info(),
                    pool:        ctx.accounts.pool.to_account_info(),
                    authority:   ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            ),
            tick_upper, upper_bitmap_word_index, liquidity as i128, true,
            pool_state.fee_growth_global_0, pool_state.fee_growth_global_1,
        )?;

        // Transfer tokens from user to vaults
        if amount_0 > 0 {
            token::transfer(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), Transfer {
                    from:      ctx.accounts.user_token_account_0.to_account_info(),
                    to:        ctx.accounts.token_vault_0.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                }),
                amount_0,
            )?;
        }
        if amount_1 > 0 {
            token::transfer(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), Transfer {
                    from:      ctx.accounts.user_token_account_1.to_account_info(),
                    to:        ctx.accounts.token_vault_1.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                }),
                amount_1,
            )?;
        }

        // CPI: update pool.liquidity via pool_core
        pool_core::cpi::modify_liquidity(
            CpiContext::new(
                ctx.accounts.pool_core_program.to_account_info(),
                pool_core::cpi::accounts::ModifyLiquidity {
                    pool: ctx.accounts.pool.to_account_info(),
                },
            ),
            liquidity as i128, tick_lower, tick_upper,
        )?;

        // Mint NFT receipt
        {
            let bump = ctx.bumps.position;
            let nft_key = ctx.accounts.nft_mint.key();
            let seeds = &[b"position".as_ref(), nft_key.as_ref(), &[bump]];
            let signer = &[&seeds[..]];
            token::mint_to(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    MintTo {
                        mint:      ctx.accounts.nft_mint.to_account_info(),
                        to:        ctx.accounts.nft_token_account.to_account_info(),
                        authority: ctx.accounts.position.to_account_info(),
                    },
                    signer,
                ),
                1,
            )?;
        }

        // Write position account
        let fee_g0 = ctx.accounts.pool.fee_growth_global_0;
        let fee_g1 = ctx.accounts.pool.fee_growth_global_1;
        let pos    = &mut ctx.accounts.position;
        pos.pool                    = ctx.accounts.pool.key();
        pos.owner                   = ctx.accounts.owner.key();
        pos.nft_mint                = ctx.accounts.nft_mint.key();
        pos.tick_lower              = tick_lower;
        pos.tick_upper              = tick_upper;
        pos.liquidity               = liquidity;
        pos.fee_growth_checkpoint_0 = fee_g0;
        pos.fee_growth_checkpoint_1 = fee_g1;
        pos.tokens_owed_0           = 0;
        pos.tokens_owed_1           = 0;
        pos.bump                    = ctx.bumps.position;

        emit!(PositionOpenedEvent {
            position: ctx.accounts.position.key(), pool: ctx.accounts.pool.key(),
            owner: ctx.accounts.owner.key(), nft_mint: ctx.accounts.nft_mint.key(),
            tick_lower, tick_upper, liquidity, amount_0, amount_1,
        });

        msg!("Position opened: L={} ticks=[{},{}] tokens=({},{})", liquidity, tick_lower, tick_upper, amount_0, amount_1);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // close_position
    // -------------------------------------------------------------------------
    pub fn close_position(
        ctx: Context<ClosePosition>,
        amount_0_minimum: u64,
        amount_1_minimum: u64,
    ) -> Result<()> {
        let tick_lower = ctx.accounts.position.tick_lower;
        let tick_upper = ctx.accounts.position.tick_upper;
        let liquidity  = ctx.accounts.position.liquidity;

        let sqrt_price_current = ctx.accounts.pool.sqrt_price;
        let sqrt_price_lower = tm_math::tick_to_sqrt_price_q64(tick_lower)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;
        let sqrt_price_upper = tm_math::tick_to_sqrt_price_q64(tick_upper)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;

        let (amount_0, amount_1) = get_amounts_for_liquidity(
            sqrt_price_current, sqrt_price_lower, sqrt_price_upper, liquidity,
        );

        let (fee_0, fee_1) = ctx.accounts.position.compute_fees_owed(
            ctx.accounts.pool.fee_growth_global_0,
            ctx.accounts.pool.fee_growth_global_1,
        );

        let total_0 = amount_0.saturating_add(fee_0).saturating_add(ctx.accounts.position.tokens_owed_0);
        let total_1 = amount_1.saturating_add(fee_1).saturating_add(ctx.accounts.position.tokens_owed_1);

        require!(total_0 >= amount_0_minimum, PositionError::SlippageExceeded);
        require!(total_1 >= amount_1_minimum, PositionError::SlippageExceeded);

        let fee_growth_0 = ctx.accounts.pool.fee_growth_global_0;
        let fee_growth_1 = ctx.accounts.pool.fee_growth_global_1;
        let lower_bitmap_word_index = bitmap_word_index_for_tick(tick_lower);
        let upper_bitmap_word_index = bitmap_word_index_for_tick(tick_upper);

        // CPI: remove lower tick
        tick_manager::cpi::update_tick(
            CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array:  ctx.accounts.tick_array_lower.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_lower.to_account_info(),
                    pool:        ctx.accounts.pool.to_account_info(),
                    authority:   ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            ),
            tick_lower, lower_bitmap_word_index, -(liquidity as i128), false, fee_growth_0, fee_growth_1,
        )?;

        // CPI: remove upper tick
        tick_manager::cpi::update_tick(
            CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array:  ctx.accounts.tick_array_upper.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_upper.to_account_info(),
                    pool:        ctx.accounts.pool.to_account_info(),
                    authority:   ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            ),
            tick_upper, upper_bitmap_word_index, -(liquidity as i128), true, fee_growth_0, fee_growth_1,
        )?;

        // CPI: update pool.liquidity
        pool_core::cpi::modify_liquidity(
            CpiContext::new(
                ctx.accounts.pool_core_program.to_account_info(),
                pool_core::cpi::accounts::ModifyLiquidity {
                    pool: ctx.accounts.pool.to_account_info(),
                },
            ),
            -(liquidity as i128), tick_lower, tick_upper,
        )?;

        // CPI: transfer tokens from vault0 to user
        if total_0 > 0 {
            pool_core::cpi::transfer_from_vault(
                CpiContext::new(
                    ctx.accounts.pool_core_program.to_account_info(),
                    pool_core::cpi::accounts::TransferFromVault {
                        pool:        ctx.accounts.pool.to_account_info(),
                        vault:       ctx.accounts.token_vault_0.to_account_info(),
                        destination: ctx.accounts.user_token_account_0.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                    },
                ),
                0, total_0,
            )?;
        }

        // CPI: transfer tokens from vault1 to user
        if total_1 > 0 {
            pool_core::cpi::transfer_from_vault(
                CpiContext::new(
                    ctx.accounts.pool_core_program.to_account_info(),
                    pool_core::cpi::accounts::TransferFromVault {
                        pool:        ctx.accounts.pool.to_account_info(),
                        vault:       ctx.accounts.token_vault_1.to_account_info(),
                        destination: ctx.accounts.user_token_account_1.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                    },
                ),
                1, total_1,
            )?;
        }

        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint:      ctx.accounts.nft_mint.to_account_info(),
                    from:      ctx.accounts.nft_token_account.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            1,
        )?;

        emit!(PositionClosedEvent {
            position: ctx.accounts.position.key(), pool: ctx.accounts.pool.key(),
            owner: ctx.accounts.owner.key(), liquidity, amount_0, amount_1,
            fees_0: fee_0, fees_1: fee_1,
        });

        msg!("Position closed: returned=({},{}) fees=({},{})", amount_0, amount_1, fee_0, fee_1);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // collect_fees
    // -------------------------------------------------------------------------
    pub fn collect_fees(ctx: Context<CollectFees>) -> Result<()> {
        let (fee_0, fee_1) = ctx.accounts.position.compute_fees_owed(
            ctx.accounts.pool.fee_growth_global_0,
            ctx.accounts.pool.fee_growth_global_1,
        );

        let total_0 = fee_0.saturating_add(ctx.accounts.position.tokens_owed_0);
        let total_1 = fee_1.saturating_add(ctx.accounts.position.tokens_owed_1);

        ctx.accounts.position.fee_growth_checkpoint_0 = ctx.accounts.pool.fee_growth_global_0;
        ctx.accounts.position.fee_growth_checkpoint_1 = ctx.accounts.pool.fee_growth_global_1;
        ctx.accounts.position.tokens_owed_0 = 0;
        ctx.accounts.position.tokens_owed_1 = 0;

        if total_0 > 0 {
            pool_core::cpi::transfer_from_vault(
                CpiContext::new(
                    ctx.accounts.pool_core_program.to_account_info(),
                    pool_core::cpi::accounts::TransferFromVault {
                        pool:          ctx.accounts.pool.to_account_info(),
                        vault:         ctx.accounts.token_vault_0.to_account_info(),
                        destination:   ctx.accounts.user_token_account_0.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                    },
                ),
                0, total_0,
            )?;
        }

        if total_1 > 0 {
            pool_core::cpi::transfer_from_vault(
                CpiContext::new(
                    ctx.accounts.pool_core_program.to_account_info(),
                    pool_core::cpi::accounts::TransferFromVault {
                        pool:          ctx.accounts.pool.to_account_info(),
                        vault:         ctx.accounts.token_vault_1.to_account_info(),
                        destination:   ctx.accounts.user_token_account_1.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                    },
                ),
                1, total_1,
            )?;
        }

        emit!(FeesCollectedEvent {
            position: ctx.accounts.position.key(),
            owner:    ctx.accounts.owner.key(),
            fees_0:   total_0, fees_1: total_1,
        });

        msg!("Fees collected: ({},{})", total_0, total_1);
        Ok(())
    }
}

// ─── Account structs ──────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(
    amount_0_desired: u64, amount_1_desired: u64,
    amount_0_minimum: u64, amount_1_minimum: u64,
    tick_lower: i32, tick_upper: i32,
)]
pub struct OpenPosition<'info> {
    #[account(
        init,
        payer = owner,
        space = Position::LEN,
        seeds = [b"position", nft_mint.key().as_ref()],
        bump
    )]
    pub position: Box<Account<'info, Position>>,

    // pool read-only — we only need to read sqrt_price and fee_growth
    // pool_core owns it; we CPI to pool_core to mutate it
    #[account(
        mut,
        constraint = pool.initialized @ PositionError::PoolNotInitialized,
        owner = pool_core_program.key() @ PositionError::InvalidPool,
    )]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    #[account(
        mut,
        constraint = token_vault_0.mint == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.mint == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    #[account(
        init, payer = owner,
        mint::decimals = 0,
        mint::authority = position,
    )]
    pub nft_mint: Box<Account<'info, Mint>>,

    #[account(
        init, payer = owner,
        associated_token::mint = nft_mint,
        associated_token::authority = owner,
    )]
    pub nft_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_0.mint  == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_0.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_1.mint  == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_1.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_1: Box<Account<'info, TokenAccount>>,

    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_array_lower: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_array_upper: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_bitmap_lower: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_bitmap_upper: UncheckedAccount<'info>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// CHECK: tick_manager program
    pub tick_manager_program: UncheckedAccount<'info>,
    /// CHECK: pool_core program — used for CPI and owner check on pool
    pub pool_core_program: UncheckedAccount<'info>,

    pub token_program:            Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program:           Program<'info, System>,
    pub rent:                     Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(
        mut,
        seeds = [b"position", position.nft_mint.as_ref()],
        bump  = position.bump,
        constraint = position.owner == owner.key() @ PositionError::Unauthorized,
        close = owner,
    )]
    pub position: Box<Account<'info, Position>>,

    #[account(
        mut,
        constraint = pool.initialized @ PositionError::PoolNotInitialized,
        owner = pool_core_program.key() @ PositionError::InvalidPool,
    )]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    #[account(
        mut,
        constraint = token_vault_0.mint == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.mint == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = nft_mint.key() == position.nft_mint @ PositionError::InvalidNft,
    )]
    pub nft_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = nft_token_account.mint   == nft_mint.key() @ PositionError::InvalidNft,
        constraint = nft_token_account.owner  == owner.key()    @ PositionError::InvalidNft,
        constraint = nft_token_account.amount == 1              @ PositionError::InvalidNft,
    )]
    pub nft_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_0.mint  == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_0.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_1.mint  == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_1.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_1: Box<Account<'info, TokenAccount>>,

    /// CHECK: tick arrays validated by tick_manager CPI
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_array_lower: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_array_upper: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_bitmap_lower: UncheckedAccount<'info>,
    /// CHECK: validated by tick_manager CPI
    #[account(mut)]
    pub tick_bitmap_upper: UncheckedAccount<'info>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// CHECK: tick_manager program
    pub tick_manager_program: UncheckedAccount<'info>,
    /// CHECK: pool_core program
    pub pool_core_program: UncheckedAccount<'info>,

    pub token_program:  Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CollectFees<'info> {
    #[account(
        mut,
        seeds = [b"position", position.nft_mint.as_ref()],
        bump  = position.bump,
        constraint = position.owner == owner.key() @ PositionError::Unauthorized,
    )]
    pub position: Box<Account<'info, Position>>,

    #[account(
        constraint = pool.initialized @ PositionError::PoolNotInitialized,
        owner = pool_core_program.key() @ PositionError::InvalidPool,
    )]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    #[account(
        mut,
        constraint = token_vault_0.mint == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.mint == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_0.mint  == pool.token_mint_0 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_0.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = user_token_account_1.mint  == pool.token_mint_1 @ PositionError::InvalidTokenAccount,
        constraint = user_token_account_1.owner == owner.key()       @ PositionError::InvalidTokenAccount,
    )]
    pub user_token_account_1: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// CHECK: pool_core program
    pub pool_core_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[error_code]
pub enum PositionError {
    #[msg("tick_lower must be < tick_upper and both multiples of TICK_SPACING")]
    InvalidTickRange,
    #[msg("Computed liquidity is zero — increase deposit amounts")]
    ZeroLiquidity,
    #[msg("Token amounts below minimum — slippage exceeded")]
    SlippageExceeded,
    #[msg("Token account mint or owner mismatch")]
    InvalidTokenAccount,
    #[msg("NFT mint or balance mismatch")]
    InvalidNft,
    #[msg("Signer does not own this position")]
    Unauthorized,
    #[msg("Pool is not initialized")]
    PoolNotInitialized,
    #[msg("Account owner is not pool_core program")]
    InvalidPool,
}

use pool_core;
