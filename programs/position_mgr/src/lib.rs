// position_mgr/src/lib.rs
//
// PROGRAM: position_mgr
//
// WHAT THIS PROGRAM DOES:
// Manages LP positions — the receipts that track who owns what liquidity.
//
// INSTRUCTIONS:
//   1. open_position    — deposit tokens, register tick range, mint NFT receipt
//   2. close_position   — burn NFT, withdraw tokens + accumulated fees
//   3. collect_fees     — collect fees without closing the position
//
// HOW IT CONNECTS TO THE OTHER PROGRAMS:
//
//   open_position calls tick_manager.update_tick via CPI
//   to register the lower and upper tick boundaries.
//   This tells the swap engine "liquidity exists in this range."
//
//   open_position updates pool.liquidity directly (if current price is in range)
//   by modifying the Pool account that pool_core owns.
//   We can do this because pool grants position_mgr write access via PDA seeds.
//
//   close_position does the reverse: remove liquidity from ticks, return tokens.
//
// THE FEE CHECKPOINT TRICK:
//   When you open a position we record pool.fee_growth_global_0/1 as a checkpoint.
//   When you close: fees_owed = (current_global - checkpoint) * your_liquidity / 2^64
//   This works without iterating all positions — O(1) per position.
//
// NOTE ON SIMPLIFIED FEE GROWTH INSIDE:
//   Full V3 computes "fee growth inside range" using tick snapshots at both boundaries.
//   We simplify: we use the global fee growth as the checkpoint.
//   This is accurate when price stays inside the range. It slightly over/under counts
//   fees when price exits and re-enters the range — acceptable for our purposes.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount, Transfer};

pub mod liquidity_math;
pub mod state;

use liquidity_math::*;
use state::*;

// Access tick_manager math constants
use tick_manager::math as tm_math;
const TICK_SPACING: i32 = 64;
declare_id!("C8QYKv68h1gSosFxrQpiZUzRnj2UTaHEYjoZ4rprekYn");

#[program]
pub mod position_mgr {
    use super::*;

    // ─────────────────────────────────────────────────────────────────────────
    // 1. open_position
    //
    // WHAT HAPPENS STEP BY STEP:
    //   a) Validate tick range (lower < upper, both multiples of tick_spacing)
    //   b) Compute liquidity L from deposited token amounts + current pool price
    //   c) CPI to tick_manager to register lower tick (+L net) and upper tick (-L net)
    //   d) Transfer tokens from user into pool vaults
    //   e) If current price is inside range: add L to pool.liquidity
    //   f) Mint 1 NFT to user's wallet
    //   g) Create Position account with fee checkpoint
    //
    // PARAMS:
    //   amount_0_desired:  max token0 user is willing to deposit
    //   amount_1_desired:  max token1 user is willing to deposit
    //   amount_0_minimum:  slippage protection — revert if deposited token0 < this
    //   amount_1_minimum:  slippage protection — revert if deposited token1 < this
    //   tick_lower:        lower price boundary (must be multiple of TICK_SPACING)
    //   tick_upper:        upper price boundary (must be multiple of TICK_SPACING)
    // ─────────────────────────────────────────────────────────────────────────
    pub fn open_position(
        ctx: Context<OpenPosition>,
        amount_0_desired: u64,
        amount_1_desired: u64,
        amount_0_minimum: u64,
        amount_1_minimum: u64,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<()> {
        // ── Validate tick range ────────────────────────────────────────────────
        require!(tick_lower < tick_upper, PositionError::InvalidTickRange);
        require!(
            tick_lower % tm_math::TICK_SPACING == 0,
            PositionError::InvalidTickRange
        );
        require!(
            tick_upper % tm_math::TICK_SPACING == 0,
            PositionError::InvalidTickRange
        );
        require!(
            tick_lower >= tm_math::MIN_TICK && tick_upper <= tm_math::MAX_TICK,
            PositionError::InvalidTickRange
        );

        let lower_array_start = tm_math::tick_to_array_start_tick(tick_lower);
        let upper_array_start = tm_math::tick_to_array_start_tick(tick_upper);
        let lower_bitmap_index = tm_math::array_start_to_bitmap_index(lower_array_start);
        let upper_bitmap_index = tm_math::array_start_to_bitmap_index(upper_array_start);
        let (lower_bitmap_word, _) = tm_math::bitmap_word_and_bit(lower_bitmap_index);
        let (upper_bitmap_word, _) = tm_math::bitmap_word_and_bit(upper_bitmap_index);

        let pool = &ctx.accounts.pool;
        let sqrt_price_current = pool.sqrt_price;
        let sqrt_price_lower = tm_math::tick_to_sqrt_price_q64(tick_lower)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;
        let sqrt_price_upper = tm_math::tick_to_sqrt_price_q64(tick_upper)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;

        // ── Compute liquidity L ────────────────────────────────────────────────
        let liquidity = get_liquidity_for_amounts(
            sqrt_price_current,
            sqrt_price_lower,
            sqrt_price_upper,
            amount_0_desired,
            amount_1_desired,
        );
        require!(liquidity > 0, PositionError::ZeroLiquidity);

        // ── Compute actual token amounts needed for this L ─────────────────────
        let (amount_0, amount_1) = get_amounts_for_liquidity(
            sqrt_price_current,
            sqrt_price_lower,
            sqrt_price_upper,
            liquidity,
        );

        require!(
            amount_0 >= amount_0_minimum,
            PositionError::SlippageExceeded
        );
        require!(
            amount_1 >= amount_1_minimum,
            PositionError::SlippageExceeded
        );

        // ── CPI: update lower tick in tick_manager ─────────────────────────────
        // Lower tick: liquidity_net = +L (entering range increases pool liquidity)
        {
            let cpi_ctx = CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array: ctx.accounts.tick_array_lower.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_lower.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            );
            tick_manager::cpi::update_tick(
                cpi_ctx,
                tick_lower,
                lower_bitmap_word,
                liquidity as i128, // positive for lower tick
                false,             // is_upper_tick = false
                pool.fee_growth_global_0,
                pool.fee_growth_global_1,
            )?;
        }

        // ── CPI: update upper tick in tick_manager ─────────────────────────────
        // Upper tick: liquidity_net = -L (exiting range decreases pool liquidity)
        {
            let cpi_ctx = CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array: ctx.accounts.tick_array_upper.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_upper.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            );
            tick_manager::cpi::update_tick(
                cpi_ctx,
                tick_upper,
                upper_bitmap_word,
                liquidity as i128, // amount (sign is flipped by is_upper_tick=true)
                true,              // is_upper_tick = true
                pool.fee_growth_global_0,
                pool.fee_growth_global_1,
            )?;
        }

        // ── Transfer tokens from user to pool vaults ───────────────────────────
        if amount_0 > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.user_token_account_0.to_account_info(),
                        to: ctx.accounts.token_vault_0.to_account_info(),
                        authority: ctx.accounts.owner.to_account_info(),
                    },
                ),
                amount_0,
            )?;
        }

        if amount_1 > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.user_token_account_1.to_account_info(),
                        to: ctx.accounts.token_vault_1.to_account_info(),
                        authority: ctx.accounts.owner.to_account_info(),
                    },
                ),
                amount_1,
            )?;
        }

        // ── Update pool liquidity if current price is inside this range ─────────
        {
            let pool_mut = &mut ctx.accounts.pool;
            let in_range =
                pool_mut.tick_current >= tick_lower && pool_mut.tick_current < tick_upper;
            if in_range {
                pool_mut.liquidity = pool_mut.liquidity.saturating_add(liquidity);
            }
        }

        // ── Mint NFT receipt (1 token, 0 decimals) ──────────────────────────────
        {
            let mint_key = ctx.accounts.nft_mint.key();
            let position_seeds = &[
                b"position".as_ref(),
                mint_key.as_ref(),
                &[ctx.bumps.position],
            ];
            let signer = &[&position_seeds[..]];

            token::mint_to(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    MintTo {
                        mint: ctx.accounts.nft_mint.to_account_info(),
                        to: ctx.accounts.nft_token_account.to_account_info(),
                        authority: ctx.accounts.position.to_account_info(),
                    },
                    signer,
                ),
                1, // exactly 1 NFT
            )?;
        }

        // ── Write Position account ─────────────────────────────────────────────
        {
            let pool = &ctx.accounts.pool;
            let position = &mut ctx.accounts.position;
            position.pool = ctx.accounts.pool.key();
            position.owner = ctx.accounts.owner.key();
            position.nft_mint = ctx.accounts.nft_mint.key();
            position.tick_lower = tick_lower;
            position.tick_upper = tick_upper;
            position.liquidity = liquidity;
            position.fee_growth_checkpoint_0 = pool.fee_growth_global_0;
            position.fee_growth_checkpoint_1 = pool.fee_growth_global_1;
            position.tokens_owed_0 = 0;
            position.tokens_owed_1 = 0;
            position.bump = ctx.bumps.position;
        }

        emit!(PositionOpenedEvent {
            position: ctx.accounts.position.key(),
            pool: ctx.accounts.pool.key(),
            owner: ctx.accounts.owner.key(),
            nft_mint: ctx.accounts.nft_mint.key(),
            tick_lower,
            tick_upper,
            liquidity,
            amount_0,
            amount_1,
        });

        msg!(
            "Position opened: L={} tick_range=[{},{}] deposited=({},{})",
            liquidity,
            tick_lower,
            tick_upper,
            amount_0,
            amount_1
        );

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 2. close_position
    //
    // Burns the NFT receipt, removes liquidity from tick boundaries,
    // returns tokens + accumulated fees to the owner.
    //
    // PARAMS:
    //   amount_0_minimum: slippage protection for token0 withdrawal
    //   amount_1_minimum: slippage protection for token1 withdrawal
    // ─────────────────────────────────────────────────────────────────────────
    pub fn close_position(
        ctx: Context<ClosePosition>,
        amount_0_minimum: u64,
        amount_1_minimum: u64,
    ) -> Result<()> {
        let position = &ctx.accounts.position;
        let pool = &ctx.accounts.pool;

        let tick_lower = position.tick_lower;
        let tick_upper = position.tick_upper;
        let liquidity = position.liquidity;
        let lower_array_start = tm_math::tick_to_array_start_tick(tick_lower);
        let upper_array_start = tm_math::tick_to_array_start_tick(tick_upper);
        let lower_bitmap_index = tm_math::array_start_to_bitmap_index(lower_array_start);
        let upper_bitmap_index = tm_math::array_start_to_bitmap_index(upper_array_start);
        let (lower_bitmap_word, _) = tm_math::bitmap_word_and_bit(lower_bitmap_index);
        let (upper_bitmap_word, _) = tm_math::bitmap_word_and_bit(upper_bitmap_index);

        let sqrt_price_current = pool.sqrt_price;
        let sqrt_price_lower = tm_math::tick_to_sqrt_price_q64(tick_lower)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;
        let sqrt_price_upper = tm_math::tick_to_sqrt_price_q64(tick_upper)
            .map_err(|_| error!(PositionError::InvalidTickRange))?;

        // ── Compute tokens to return ───────────────────────────────────────────
        let (amount_0, amount_1) = get_amounts_for_liquidity(
            sqrt_price_current,
            sqrt_price_lower,
            sqrt_price_upper,
            liquidity,
        );

        // ── Compute fees owed ──────────────────────────────────────────────────
        // Simplified: use global fee growth as fee_growth_inside
        // (accurate when price hasn't left the range since deposit)
        let (fee_0, fee_1) =
            position.compute_fees_owed(pool.fee_growth_global_0, pool.fee_growth_global_1);

        let total_0 = amount_0
            .saturating_add(fee_0)
            .saturating_add(position.tokens_owed_0);
        let total_1 = amount_1
            .saturating_add(fee_1)
            .saturating_add(position.tokens_owed_1);

        require!(total_0 >= amount_0_minimum, PositionError::SlippageExceeded);
        require!(total_1 >= amount_1_minimum, PositionError::SlippageExceeded);

        // ── CPI: remove liquidity from lower tick ──────────────────────────────
        {
            let neg_liquidity = -(liquidity as i128);
            let cpi_ctx = CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array: ctx.accounts.tick_array_lower.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_lower.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            );
            tick_manager::cpi::update_tick(
                cpi_ctx,
                tick_lower,
                lower_bitmap_word,
                neg_liquidity,
                false,
                pool.fee_growth_global_0,
                pool.fee_growth_global_1,
            )?;
        }

        // ── CPI: remove liquidity from upper tick ──────────────────────────────
        {
            let neg_liquidity = -(liquidity as i128);
            let cpi_ctx = CpiContext::new(
                ctx.accounts.tick_manager_program.to_account_info(),
                tick_manager::cpi::accounts::UpdateTick {
                    tick_array: ctx.accounts.tick_array_upper.to_account_info(),
                    tick_bitmap: ctx.accounts.tick_bitmap_upper.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
            );
            tick_manager::cpi::update_tick(
                cpi_ctx,
                tick_upper,
                upper_bitmap_word,
                neg_liquidity,
                true,
                pool.fee_growth_global_0,
                pool.fee_growth_global_1,
            )?;
        }

        // ── Update pool liquidity if in range ──────────────────────────────────
        {
            let pool_mut = &mut ctx.accounts.pool;
            let in_range =
                pool_mut.tick_current >= tick_lower && pool_mut.tick_current < tick_upper;
            if in_range {
                pool_mut.liquidity = pool_mut.liquidity.saturating_sub(liquidity);
            }
        }

        // ── Transfer tokens back to user ───────────────────────────────────────
        // Pool vaults are owned by pool PDA — we sign with pool seeds
        let pool_account = &ctx.accounts.pool;
        let mint_0_key = pool_account.token_mint_0;
        let mint_1_key = pool_account.token_mint_1;
        let fee_rate_bytes = pool_account.fee_rate.to_le_bytes();
        let pool_bump = pool_account.bump;
        let pool_seeds = &[
            b"pool".as_ref(),
            mint_0_key.as_ref(),
            mint_1_key.as_ref(),
            fee_rate_bytes.as_ref(),
            &[pool_bump],
        ];
        let signer_seeds = &[&pool_seeds[..]];

        if total_0 > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_0.to_account_info(),
                        to: ctx.accounts.user_token_account_0.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    signer_seeds,
                ),
                total_0,
            )?;
        }

        if total_1 > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_1.to_account_info(),
                        to: ctx.accounts.user_token_account_1.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    signer_seeds,
                ),
                total_1,
            )?;
        }

        // ── Burn NFT ────────────────────────────────────────────────────────────

        {
            let mint_key = ctx.accounts.nft_mint.key();
            let position_seeds = &[b"position".as_ref(), mint_key.as_ref(), &[position.bump]];
            let signer = &[&position_seeds[..]];

            token::burn(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Burn {
                        mint: ctx.accounts.nft_mint.to_account_info(),
                        from: ctx.accounts.nft_token_account.to_account_info(),
                        authority: ctx.accounts.position.to_account_info(),
                    },
                    signer,
                ),
                1,
            )?;
        }

        emit!(PositionClosedEvent {
            position: ctx.accounts.position.key(),
            pool: ctx.accounts.pool.key(),
            owner: ctx.accounts.owner.key(),
            liquidity,
            amount_0,
            amount_1,
            fees_0: fee_0,
            fees_1: fee_1,
        });

        msg!(
            "Position closed: returned=({},{}) fees=({},{})",
            amount_0,
            amount_1,
            fee_0,
            fee_1
        );

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 3. collect_fees
    //
    // Collect accumulated fees without touching the liquidity.
    // The position stays open. tokens_owed_0/1 is zeroed after collection.
    // ─────────────────────────────────────────────────────────────────────────
    pub fn collect_fees(ctx: Context<CollectFees>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let position = &mut ctx.accounts.position;

        // Compute fees since last checkpoint
        let (fee_0, fee_1) =
            position.compute_fees_owed(pool.fee_growth_global_0, pool.fee_growth_global_1);

        let total_0 = fee_0.saturating_add(position.tokens_owed_0);
        let total_1 = fee_1.saturating_add(position.tokens_owed_1);

        // Update checkpoint and clear owed
        position.fee_growth_checkpoint_0 = pool.fee_growth_global_0;
        position.fee_growth_checkpoint_1 = pool.fee_growth_global_1;
        position.tokens_owed_0 = 0;
        position.tokens_owed_1 = 0;

        // Transfer fees from pool vaults to user
        let mint_0_key = pool.token_mint_0;
        let mint_1_key = pool.token_mint_1;
        let fee_rate_bytes = pool.fee_rate.to_le_bytes();
        let pool_bump = pool.bump;
        let pool_seeds = &[
            b"pool".as_ref(),
            mint_0_key.as_ref(),
            mint_1_key.as_ref(),
            fee_rate_bytes.as_ref(),
            &[pool_bump],
        ];
        let signer_seeds = &[&pool_seeds[..]];

        if total_0 > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_0.to_account_info(),
                        to: ctx.accounts.user_token_account_0.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    signer_seeds,
                ),
                total_0,
            )?;
        }

        if total_1 > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.token_vault_1.to_account_info(),
                        to: ctx.accounts.user_token_account_1.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    signer_seeds,
                ),
                total_1,
            )?;
        }

        emit!(FeesCollectedEvent {
            position: ctx.accounts.position.key(),
            owner: ctx.accounts.owner.key(),
            fees_0: total_0,
            fees_1: total_1,
        });

        msg!("Fees collected: ({},{})", total_0, total_1);
        Ok(())
    }
}

// ─── Account structs ──────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(
    amount_0_desired: u64,
    amount_1_desired: u64,
    amount_0_minimum: u64,
    amount_1_minimum: u64,
    tick_lower: i32,
    tick_upper: i32,
)]
pub struct OpenPosition<'info> {
    // Position PDA — seeded by ["position", nft_mint]
    #[account(
        init,
        payer = owner,
        space = Position::LEN,
        seeds = [b"position", nft_mint.key().as_ref()],
        bump
    )]
    pub position: Box<Account<'info, Position>>,

    // Pool — mutable because we update pool.liquidity
    #[account(mut)]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    // Pool's token vaults (destinations for deposited tokens)
    #[account(
        mut,
        constraint = token_vault_0.key() == pool.token_vault_0 @ PositionError::InvalidTokenAccount
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.key() == pool.token_vault_1 @ PositionError::InvalidTokenAccount
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    // NFT mint — created fresh for this position (0 decimals, supply will be 1)
    #[account(
        init,
        payer = owner,
        mint::decimals = 0,
        mint::authority = position,  // position PDA is the mint authority
    )]
    pub nft_mint: Box<Account<'info, Mint>>,

    // Owner's NFT token account — will receive the minted NFT
    #[account(
        init,
        payer = owner,
        associated_token::mint = nft_mint,
        associated_token::authority = owner,
    )]
    pub nft_token_account: Box<Account<'info, TokenAccount>>,

    // Owner's token accounts (source of deposited tokens)
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

    /// CHECK: Validated by the tick_manager CPI via PDA seeds for the lower tick array.
    #[account(mut)]
    pub tick_array_lower: UncheckedAccount<'info>,

    /// CHECK: Validated by the tick_manager CPI via PDA seeds for the upper tick array.
    #[account(mut)]
    pub tick_array_upper: UncheckedAccount<'info>,

    /// CHECK: Validated by the tick_manager CPI as the bitmap account for the lower tick.
    #[account(mut)]
    pub tick_bitmap_lower: UncheckedAccount<'info>,

    /// CHECK: Validated by the tick_manager CPI as the bitmap account for the upper tick.
    #[account(mut)]
    pub tick_bitmap_upper: UncheckedAccount<'info>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub tick_manager_program: Program<'info, tick_manager::program::TickManager>,

    pub pool_core_program: Program<'info, pool_core::program::PoolCore>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(
        mut,
        seeds = [b"position", position.nft_mint.as_ref()],
        bump  = position.bump,
        constraint = position.owner == owner.key() @ PositionError::Unauthorized,
        close = owner,  // rent goes back to owner when account closed
    )]
    pub position: Box<Account<'info, Position>>,

    #[account(mut)]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    #[account(
        mut,
        constraint = token_vault_0.key() == pool.token_vault_0 @ PositionError::InvalidTokenAccount
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.key() == pool.token_vault_1 @ PositionError::InvalidTokenAccount
    )]
    pub token_vault_1: Box<Account<'info, TokenAccount>>,

    // NFT mint — will have supply reduced to 0 after burn
    #[account(
        mut,
        constraint = nft_mint.key() == position.nft_mint @ PositionError::InvalidNft,
    )]
    pub nft_mint: Box<Account<'info, Mint>>,

    // Owner's NFT account — NFT burned from here
    #[account(
        mut,
        constraint = nft_token_account.mint   == nft_mint.key()  @ PositionError::InvalidNft,
        constraint = nft_token_account.owner  == owner.key()     @ PositionError::InvalidNft,
        constraint = nft_token_account.amount == 1               @ PositionError::InvalidNft,
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

    /// CHECK: Validated by the tick_manager CPI via PDA seeds for the lower tick array.
    #[account(mut)]
    pub tick_array_lower: UncheckedAccount<'info>,
    /// CHECK: Validated by the tick_manager CPI via PDA seeds for the upper tick array.
    #[account(mut)]
    pub tick_array_upper: UncheckedAccount<'info>,
    /// CHECK: Validated by the tick_manager CPI as the bitmap account for the lower tick.
    #[account(mut)]
    pub tick_bitmap_lower: UncheckedAccount<'info>,
    /// CHECK: Validated by the tick_manager CPI as the bitmap account for the upper tick.
    #[account(mut)]
    pub tick_bitmap_upper: UncheckedAccount<'info>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub tick_manager_program: Program<'info, tick_manager::program::TickManager>,
    pub pool_core_program: Program<'info, pool_core::program::PoolCore>,

    pub token_program: Program<'info, Token>,
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

    #[account()]
    pub pool: Box<Account<'info, pool_core::state::Pool>>,

    #[account(
        mut,
        constraint = token_vault_0.key() == pool.token_vault_0 @ PositionError::InvalidTokenAccount
    )]
    pub token_vault_0: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = token_vault_1.key() == pool.token_vault_1 @ PositionError::InvalidTokenAccount
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

    pub pool_core_program: Program<'info, pool_core::program::PoolCore>,

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
}

// Access pool_core state types for Pool account cross-program reference
use pool_core;
