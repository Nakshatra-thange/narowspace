//! state.rs — Account data structures for tick_manager
//!
//! WHAT THIS FILE CONTAINS:
//! The three on-chain accounts this program manages:
//!   1. TickArray  — holds 88 consecutive tick slots
//!   2. TickBitmap — one word (u64) tracking 8 TickArrays
//!
//! WHY THESE TWO ACCOUNTS?
//! We can't have one account per tick — there are millions of possible ticks.
//! TickArray batches 88 ticks into one account (1 Solana account per 88 ticks).
//! TickBitmap indexes those arrays so the swap engine can skip empty regions fast.

use anchor_lang::prelude::*;
use crate::math::{TICKS_PER_ARRAY, ARRAYS_PER_BITMAP_WORD};


// ─── TickData ──────────────────────────────────────────────────────────────────

/// One slot inside a TickArray. Represents a single price checkpoint.
///
/// FIELDS EXPLAINED:
///
/// liquidity_net:
///   When the swap price CROSSES this tick, pool liquidity changes by this amount.
///   Positive at the lower tick of a position (entering the range = add liquidity).
///   Negative at the upper tick (leaving the range = subtract liquidity).
///   Example: LP adds 1000 units between tick 100–200:
///     tick 100: liquidity_net = +1000
///     tick 200: liquidity_net = -1000
///
/// liquidity_gross:
///   Total liquidity that references this tick (lower OR upper bound).
///   Used to know if a tick has any active positions — if 0, the tick is empty.
///
/// fee_growth_outside_0/1:
///   Fee accumulator snapshot for token0 and token1.
///   Stores how much fee has been earned per unit of liquidity OUTSIDE this tick's range.
///   Used to calculate how much fee an individual LP earned inside their range.
///   (Simplified model: we use this for correctness but don't compute cross-tick splits.)
///
/// initialized:
///   True if at least one LP position references this tick.
///   Uninitialized ticks are skipped during swap.
#[zero_copy]
#[repr(C)]
pub struct TickData {
    pub liquidity_net:         i128,  // signed: +add or -remove liquidity when crossed
    pub liquidity_gross:       u128,  // total references to this tick
    pub fee_growth_outside_0:  u128,  // fee accumulator for token0
    pub fee_growth_outside_1:  u128,  // fee accumulator for token1
    pub initialized:           u8,
    pub _padding:              [u8; 15], // align to 16 bytes
}

// ─── TickArray ─────────────────────────────────────────────────────────────────

/// Holds 88 consecutive tick slots.
///
/// PDA SEEDS: ["tick_array", pool_pubkey, start_tick_index (as 4 bytes)]
///
/// WHY 88?
/// Orca Whirlpools uses 88. It's a balance between:
///   - Account size (larger = more rent)
///   - Number of CPIs per swap (smaller arrays = more CPI calls to cross ranges)
/// 88 ticks × 64 spacing = 5,632 price units per array.
///
/// start_tick_index:
///   The tick index of the FIRST slot in this array.
///   Must be a multiple of (TICKS_PER_ARRAY × TICK_SPACING).
///   All ticks in this array: start_tick_index, start_tick_index+64, ..., start_tick_index+5568
///
/// pool: which pool this array belongs to (one pool = its own set of tick arrays)
#[account(zero_copy)]

#[repr(C)]
pub struct TickArray {
    pub start_tick_index: i32,
    pub _padding0: [u8; 4],        // align start_tick to 8 bytes
    pub pool: Pubkey,              // 32 bytes
    pub _padding1: [u8; 8],        // align tick storage to 16 bytes
    pub ticks: [[TickData; 11]; 8], // 88 × 64 bytes = 5,632 bytes
}

impl TickArray {
    /// Space needed for this account (discriminator + fields).
    /// discriminator: 8 bytes
    /// start_tick_index: 4 bytes + 4 padding
    /// pool: 32 bytes
    /// ticks: 88 × 64 bytes = 5,632 bytes
    pub const LEN: usize = 8 + 4 + 4 + 32 + 8 + (88 * 80);

    /// Get the tick data at a given absolute tick index.
    /// Validates that the tick belongs to this array.
    pub fn get_tick(&self, tick_index: i32) -> Option<&TickData> {
        let array_size = TICKS_PER_ARRAY as i32 * crate::math::TICK_SPACING;
        if tick_index < self.start_tick_index || tick_index >= self.start_tick_index + array_size {
            return None; // tick doesn't belong to this array
        }
        let offset = (tick_index - self.start_tick_index) / crate::math::TICK_SPACING;
        let offset = offset as usize;
        let chunk = offset / 11;
        let slot = offset % 11;
        self.ticks.get(chunk).and_then(|ticks| ticks.get(slot))
    }

    /// Mutable version of get_tick.
    pub fn get_tick_mut(&mut self, tick_index: i32) -> Option<&mut TickData> {
        let array_size = TICKS_PER_ARRAY as i32 * crate::math::TICK_SPACING;
        if tick_index < self.start_tick_index || tick_index >= self.start_tick_index + array_size {
            return None;
        }
        let offset = (tick_index - self.start_tick_index) / crate::math::TICK_SPACING;
        let offset = offset as usize;
        let chunk = offset / 11;
        let slot = offset % 11;
        self.ticks.get_mut(chunk).and_then(|ticks| ticks.get_mut(slot))
    }
}

// ─── TickBitmap ────────────────────────────────────────────────────────────────

/// One bitmap word: tracks 8 consecutive TickArrays.
///
/// PDA SEEDS: ["tick_bitmap", pool_pubkey, word_index (as 4 bytes)]
///
/// HOW IT WORKS:
/// This is a u8 where bit i = 1 means "TickArray at position (word_index×8 + i)
/// has at least one initialized tick."
///
/// During a swap, instead of loading every TickArray account (expensive),
/// we load the bitmap word and do bit operations to find the next non-empty array.
///
/// word_index:
///   Which group of 8 arrays this bitmap word covers.
///   word_index=0 covers array indices 0–7
///   word_index=1 covers array indices 8–15
///   etc.
///
/// initialized_arrays: u8 where bit i=1 means array (word_index*8 + i) has ticks.
#[account]
pub struct TickBitmap {
    pub pool: Pubkey,
    pub word_index: i32,
    pub initialized_arrays: u8,  // 8 bits = 8 arrays
    pub _padding: [u8; 3],
}

impl TickBitmap {
    pub const LEN: usize = 8 + 32 + 4 + 1 + 3;

    /// Set bit at position `bit_index` (0–7) to indicate that array is initialized.
    pub fn set_bit(&mut self, bit_index: u32) {
        self.initialized_arrays |= 1u8 << bit_index;
    }

    /// Clear bit at position `bit_index`.
    pub fn clear_bit(&mut self, bit_index: u32) {
        self.initialized_arrays &= !(1u8 << bit_index);
    }

    /// Check if bit at position `bit_index` is set.
    pub fn is_set(&self, bit_index: u32) -> bool {
        (self.initialized_arrays >> bit_index) & 1 == 1
    }

    /// Find the next set bit at or after `from_bit` (searching right/higher).
    /// Returns None if no set bit found.
    pub fn next_initialized_array_from(&self, from_bit: u32) -> Option<u32> {
        for i in from_bit..ARRAYS_PER_BITMAP_WORD as u32 {
            if self.is_set(i) {
                return Some(i);
            }
        }
        None
    }

    /// Find the next set bit at or before `from_bit` (searching left/lower).
    /// Used for swaps going in the negative price direction.
    pub fn prev_initialized_array_from(&self, from_bit: u32) -> Option<u32> {
        for i in (0..=from_bit).rev() {
            if self.is_set(i) {
                return Some(i);
            }
        }
        None
    }
}
