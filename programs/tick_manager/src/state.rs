use anchor_lang::prelude::*;
use crate::math::{TICKS_PER_ARRAY, ARRAYS_PER_BITMAP_WORD};

#[zero_copy] 
#[repr(C)]
pub struct TickData {
    pub liquidity_net:         i128,  
    pub liquidity_gross:       u128,  
    pub fee_growth_outside_0:  u128,  
    pub fee_growth_outside_1:  u128,  
    pub initialized:           u8,
    pub _padding:              [u8; 15], 
}

#[account(zero_copy)]

#[repr(C)]
pub struct TickArray {
    pub start_tick_index: i32,
    pub _padding0: [u8; 4],        
    pub pool: Pubkey,              
    pub _padding1: [u8; 8],        
    pub ticks: [[TickData; 11]; 8], 
}

impl TickArray {
    pub const LEN: usize = 8 + 4 + 4 + 32 + 8 + (88 * 80);

    pub fn get_tick(&self, tick_index: i32) -> Option<&TickData> {
        let array_size = TICKS_PER_ARRAY as i32 * crate::math::TICK_SPACING;
        if tick_index < self.start_tick_index || tick_index >= self.start_tick_index + array_size {
            return None; 
        }
        let offset = (tick_index - self.start_tick_index) / crate::math::TICK_SPACING;
        let offset = offset as usize;
        let chunk = offset / 11;
        let slot = offset % 11;
        self.ticks.get(chunk).and_then(|ticks| ticks.get(slot))
    }

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

#[account]
pub struct TickBitmap {
    pub pool: Pubkey,
    pub word_index: i32,
    pub initialized_arrays: u8, 
    pub _padding: [u8; 3],
}

impl TickBitmap {
    pub const LEN: usize = 8 + 32 + 4 + 1 + 3;

    pub fn set_bit(&mut self, bit_index: u32) {
        self.initialized_arrays |= 1u8 << bit_index;
    }

    pub fn clear_bit(&mut self, bit_index: u32) {
        self.initialized_arrays &= !(1u8 << bit_index);
    }

    pub fn is_set(&self, bit_index: u32) -> bool {
        (self.initialized_arrays >> bit_index) & 1 == 1
    }

    pub fn next_initialized_array_from(&self, from_bit: u32) -> Option<u32> {
        for i in from_bit..ARRAYS_PER_BITMAP_WORD as u32 {
            if self.is_set(i) {
                return Some(i);
            }
        }
        None
    }

    pub fn prev_initialized_array_from(&self, from_bit: u32) -> Option<u32> {
        for i in (0..=from_bit).rev() {
            if self.is_set(i) {
                return Some(i);
            }
        }
        None
    }
}
