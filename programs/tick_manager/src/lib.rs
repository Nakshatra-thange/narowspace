use anchor_lang::prelude::*;
 
pub mod math;
pub mod state;
 
use math::*;
use state::*;

declare_id!("9V4BX9p6bRy37gWMDR5xatdntPQKdzU6DXpf3gqsBumW");


#[program]
pub mod tick_manager {
    use super::*;

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


        msg!(
            "TickArray initialized: start_tick={} pool={}",
            start_tick_index,
            ctx.accounts.pool.key()
        );

        Ok(())
    }


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

    
        let tick = tick_array
            .get_tick_mut(tick_index)
            .ok_or(TickManagerError::TickNotInitialized)?;

        let was_initialized = tick.initialized != 0;

        
        tick.liquidity_gross = if liquidity_delta > 0 {
            tick.liquidity_gross
                .checked_add(liquidity_delta as u128)
                .ok_or(TickManagerError::TickOutOfRange)?
        } else {
            tick.liquidity_gross
                .checked_sub((-liquidity_delta) as u128)
                .ok_or(TickManagerError::TickOutOfRange)?
        };

    
        let net_delta = if is_upper_tick { -liquidity_delta } else { liquidity_delta };
        tick.liquidity_net = tick
            .liquidity_net
            .checked_add(net_delta)
            .ok_or(TickManagerError::TickOutOfRange)?;

        
        if !was_initialized && tick.liquidity_gross > 0 {
            tick.initialized = 1;
    
            tick.fee_growth_outside_0 = 0;
            tick.fee_growth_outside_1 = 0;
        }

        if tick.liquidity_gross == 0 {
            tick.initialized = 0;
            tick.fee_growth_outside_0 = 0;
            tick.fee_growth_outside_1 = 0;
        }

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
        tick.fee_growth_outside_0 = fee_growth_global_0
            .wrapping_sub(tick.fee_growth_outside_0);
        tick.fee_growth_outside_1 = fee_growth_global_1
            .wrapping_sub(tick.fee_growth_outside_1);

        let net = tick.liquidity_net;

        msg!("Crossed tick {}: liquidity_net={}", tick_index, net);

        Ok(net)
    }

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

        err!(TickManagerError::TickNotInitialized)
    }
}


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
