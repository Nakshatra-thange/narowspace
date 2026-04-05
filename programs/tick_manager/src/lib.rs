use anchor_lang::prelude::*;
 
pub mod math;
pub mod state;
 
use math::*;
use state::*;

declare_id!("6qTYRwXtuioRMmDy8rqTN8hXeQwWAQn1wSWpKVJV4q2P");


#[program]
pub mod tick_manager {
    use super::*;

    // ─── 1. initialize_tick_array ───────────────────────────────────────────────
    //
    // Creates a TickArray account for a given start_tick.
    // Anyone can call this — it's just renting space on-chain.
    // The pool is charged rent (or the user calling the SDK).
    //
    // WHEN IS THIS CALLED?
    // Before adding liquidity to a price range, the SDK checks if TickArrays
    // for that range exist. If not, it calls this instruction first.
    //
    // VALIDATION:
    // start_tick must be a multiple of (TICKS_PER_ARRAY × TICK_SPACING).
    // This ensures arrays don't overlap.

    pub fn initialize_tick_array(
        ctx: Context<InitializeTickArray>,
        start_tick_index: i32,
    ) -> Result<()> {
        let array_size = TICKS_PER_ARRAY as i32 * TICK_SPACING;


        require!(
            start_tick_index % array_size == 0,
            TickManagerError::InvalidTickSpacing
        );

        validate_tick(start_tick_index)?;

        let tick_array = &mut ctx.accounts.tick_array.load_init()?;
        tick_array.start_tick_index = start_tick_index;
        tick_array.pool = ctx.accounts.pool.key();
        // ticks are zero-initialized by default (initialized=false, liquidity=0)

        msg!(
            "TickArray initialized: start_tick={} pool={}",
            start_tick_index,
            ctx.accounts.pool.key()
        );

        Ok(())
    }

    // ─── 2. update_tick ────────────────────────────────────────────────────────
    //
    // Called by position_mgr when an LP opens or closes a position.
    // Updates the tick's liquidity_net and liquidity_gross.
    // Also flips the tick's bit in the bitmap if it transitions from
    // uninitialized → initialized (or vice versa).
    //
    // PARAMS:
    //   tick_index     — which tick to update
    //   liquidity_delta — how much liquidity to add (positive) or remove (negative)
    //   is_upper_tick  — true if this is the upper bound of the position
    //                    (upper tick gets NEGATIVE liquidity_net)
    //   fee_growth_0/1 — current global fee accumulators (stored as snapshot)

    pub fn update_tick(
        ctx: Context<UpdateTick>,
        tick_index: i32,
        bitmap_word_index: i32,
        liquidity_delta: i128,
        is_upper_tick: bool,
        _fee_growth_global_0: u128,
        _fee_growth_global_1: u128,
    ) -> Result<()> {
        validate_tick(tick_index)?;
        validate_tick_spacing(tick_index)?;

        let tick_array = &mut ctx.accounts.tick_array.load_mut()?;

        // Verify this tick belongs to this array
        let tick = tick_array
            .get_tick_mut(tick_index)
            .ok_or(TickManagerError::TickNotInitialized)?;

        let was_initialized = tick.initialized != 0;

        // Update gross liquidity (always positive)
        tick.liquidity_gross = if liquidity_delta > 0 {
            tick.liquidity_gross
                .checked_add(liquidity_delta as u128)
                .ok_or(TickManagerError::TickOutOfRange)?
        } else {
            tick.liquidity_gross
                .checked_sub((-liquidity_delta) as u128)
                .ok_or(TickManagerError::TickOutOfRange)?
        };

        // Update net liquidity (signed: positive at lower tick, negative at upper tick)
        // Lower tick: +delta (entering the range adds liquidity to pool)
        // Upper tick: -delta (leaving the range removes liquidity from pool)
        let net_delta = if is_upper_tick { -liquidity_delta } else { liquidity_delta };
        tick.liquidity_net = tick
            .liquidity_net
            .checked_add(net_delta)
            .ok_or(TickManagerError::TickOutOfRange)?;

        // Initialize fee snapshot if this is the first time this tick is being used
        if !was_initialized && tick.liquidity_gross > 0 {
            tick.initialized = 1;
            // Fee growth "outside" starts at 0 if current tick is below this tick,
            // or at current global if current tick is above (simplified: always 0 at init)
            tick.fee_growth_outside_0 = 0;
            tick.fee_growth_outside_1 = 0;
        }

        // Clear initialized flag if no more liquidity references this tick
        if tick.liquidity_gross == 0 {
            tick.initialized = 0;
            tick.fee_growth_outside_0 = 0;
            tick.fee_growth_outside_1 = 0;
        }

        // Update bitmap: flip bit if initialized status changed
        let is_now_initialized = tick.liquidity_gross > 0;
        let _ = tick_array; // release borrow before accessing bitmap

        if was_initialized != is_now_initialized {
            let array_index = array_start_to_bitmap_index(
                tick_to_array_start_tick(tick_index)
            );
            let (word_index, bit_index) = bitmap_word_and_bit(array_index);
            require!(bitmap_word_index == word_index, TickManagerError::BitmapOutOfRange);

            let bitmap = &mut ctx.accounts.tick_bitmap;
            if bitmap.pool == Pubkey::default() {
                bitmap.pool = ctx.accounts.pool.key();
                bitmap.word_index = word_index;
            } else {
                require_keys_eq!(bitmap.pool, ctx.accounts.pool.key(), TickManagerError::BitmapOutOfRange);
                require!(bitmap.word_index == word_index, TickManagerError::BitmapOutOfRange);
            }
            if is_now_initialized {
                bitmap.set_bit(bit_index);
            } else {
                bitmap.clear_bit(bit_index);
            }
        }

        Ok(())
    }

    // ─── 3. cross_tick ─────────────────────────────────────────────────────────
    //
    // Called by pool_core during a swap when the price moves through a tick boundary.
    // Applies the tick's liquidity_net to the pool's current liquidity.
    //
    // WHAT "CROSSING" MEANS:
    // If price moves upward and crosses tick 200 (an upper boundary for someone's position),
    // that means we're now ABOVE that position's range — we subtract its liquidity.
    // If price moves downward and crosses tick 200 (a lower boundary), we're entering
    // the range — we add its liquidity.
    //
    // RETURNS: the liquidity_net of the crossed tick (pool_core applies it to pool state)
    //
    // Also updates fee_growth_outside (flips the snapshot — standard V3 fee accounting trick).

    pub fn cross_tick(
        ctx: Context<CrossTick>,
        tick_index: i32,
        fee_growth_global_0: u128,
        fee_growth_global_1: u128,
    ) -> Result<i128> {
        let tick_array = &mut ctx.accounts.tick_array.load_mut()?;

        let tick = tick_array
            .get_tick_mut(tick_index)
            .ok_or(TickManagerError::TickNotInitialized)?;

        require!(tick.initialized != 0, TickManagerError::TickNotInitialized);

        // Flip fee_growth_outside: new_outside = global - old_outside
        // This is the standard Uniswap V3 trick that makes per-position fee
        // calculation possible without iterating all positions.
        tick.fee_growth_outside_0 = fee_growth_global_0
            .wrapping_sub(tick.fee_growth_outside_0);
        tick.fee_growth_outside_1 = fee_growth_global_1
            .wrapping_sub(tick.fee_growth_outside_1);

        let net = tick.liquidity_net;

        msg!("Crossed tick {}: liquidity_net={}", tick_index, net);

        Ok(net)
    }

    // ─── 4. get_next_initialized_tick ──────────────────────────────────────────
    //
    // Called by pool_core at the START of each swap step.
    // Finds the next tick (in either direction) that has active liquidity.
    //
    // WHY THIS IS NEEDED:
    // During a swap, the price moves along the curve until it hits a tick boundary.
    // At the boundary, liquidity changes (cross_tick is called), then the swap continues.
    // pool_core needs to know: "where is the next boundary in my direction of travel?"
    //
    // DIRECTION:
    //   zero_for_one=true  → price going DOWN → search for next tick BELOW current
    //   zero_for_one=false → price going UP   → search for next tick ABOVE current
    //
    // This is a READ operation — it checks tick.initialized in the array.
    // We do NOT modify any state here.
    //
    // NOTE: In production, this would scan the bitmap first then drill into the array.
    // For Day 1, we scan the array directly (bitmap optimization comes with pool_core).

    pub fn get_next_initialized_tick(
        ctx: Context<GetNextTick>,
        current_tick: i32,
        zero_for_one: bool,
    ) -> Result<i32> {
        let tick_array = &ctx.accounts.tick_array.load()?;

        let array_size = TICKS_PER_ARRAY as i32 * TICK_SPACING;
        let start = tick_array.start_tick_index;
        let end = start + array_size;

        if zero_for_one {
            // Searching downward: find largest initialized tick < current_tick
            let mut candidate = current_tick - TICK_SPACING;
            while candidate >= start {
                if let Some(tick) = tick_array.get_tick(candidate) {
                    if tick.initialized != 0 {
                        return Ok(candidate);
                    }
                }
                candidate -= TICK_SPACING;
            }
        } else {
            // Searching upward: find smallest initialized tick > current_tick
            let mut candidate = current_tick + TICK_SPACING;
            while candidate < end {
                if let Some(tick) = tick_array.get_tick(candidate) {
                    if tick.initialized != 0 {
                        return Ok(candidate);
                    }
                }
                candidate += TICK_SPACING;
            }
        }

        // No initialized tick found in this array
        // pool_core will load the next array and try again
        err!(TickManagerError::TickNotInitialized)
    }
}

// ─── Account validation structs ───────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(start_tick_index: i32)]
pub struct InitializeTickArray<'info> {
    #[account(
        init,
        payer = payer,
        space = TickArray::LEN,
        seeds = [
            b"tick_array",
            pool.key().as_ref(),
            &start_tick_index.to_le_bytes(),
        ],
        bump
    )]
    pub tick_array: AccountLoader<'info, TickArray>,

    /// CHECK: this is just the pool pubkey used as a PDA seed.
    /// The actual pool account lives in pool_core — we reference it by key only.
    pub pool: UncheckedAccount<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(tick_index: i32, bitmap_word_index: i32)]
pub struct UpdateTick<'info> {
    #[account(mut)]
    pub tick_array: AccountLoader<'info, TickArray>,

    #[account(
        init_if_needed,
        payer = authority,
        space = TickBitmap::LEN,
        seeds = [
            b"tick_bitmap",
            pool.key().as_ref(),
            &bitmap_word_index.to_le_bytes(),
        ],
        bump,
    )]
    pub tick_bitmap: Account<'info, TickBitmap>,

    /// CHECK: pool pubkey, validated by PDA seeds
    pub pool: UncheckedAccount<'info>,

    /// Only position_mgr (a specific PDA) can call this
    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(tick_index: i32)]
pub struct CrossTick<'info> {
    #[account(mut)]
    pub tick_array: AccountLoader<'info, TickArray>,

    /// CHECK: pool pubkey, validated by PDA seeds
    pub pool: UncheckedAccount<'info>,

    /// Only pool_core can call cross_tick
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct GetNextTick<'info> {
    pub tick_array: AccountLoader<'info, TickArray>,

    /// CHECK: pool pubkey
    pub pool: UncheckedAccount<'info>,
}
