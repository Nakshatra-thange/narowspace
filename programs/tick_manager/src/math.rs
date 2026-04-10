
use anchor_lang::prelude::*;

pub const TICK_SPACING: i32 = 64;
pub const TICKS_PER_ARRAY: usize = 88;
pub const ARRAYS_PER_BITMAP_WORD: usize = 8;
pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;

pub const Q64_RESOLUTION: u128 = 1u128 << 64;
pub fn validate_tick(tick: i32) -> Result<()> {
    require!(tick >= MIN_TICK && tick <= MAX_TICK, TickManagerError::TickOutOfRange);
    Ok(())
}

pub fn validate_tick_spacing(tick: i32) -> Result<()> {
    require!(tick % TICK_SPACING == 0, TickManagerError::InvalidTickSpacing);
    Ok(())
}

pub fn tick_to_sqrt_price_q64(tick: i32) -> Result<u128> {
    validate_tick(tick)?;
   
    let sqrt_price = 1.0001_f64.powf(tick as f64 / 2.0);
    let scaled = sqrt_price * Q64_RESOLUTION as f64;

    require!(
        scaled.is_finite() && scaled >= 0.0 && scaled <= u128::MAX as f64,
        TickManagerError::TickOutOfRange
    );

    Ok(scaled.floor() as u128)
}

pub fn tick_to_array_start_tick(tick: i32) -> i32 {
    let array_size = (TICKS_PER_ARRAY as i32) * TICK_SPACING;
   
    let div = tick / array_size;
    let rem = tick % array_size;
    (if rem < 0 { div - 1 } else { div }) * array_size
}

pub fn array_start_to_bitmap_index(start_tick: i32) -> i32 {
    let array_size = (TICKS_PER_ARRAY as i32) * TICK_SPACING;
    let min_array_start = tick_to_array_start_tick(MIN_TICK);
    (start_tick - min_array_start) / array_size
}

pub fn bitmap_word_and_bit(array_index: i32) -> (i32, u32) {
    let word_index = array_index / ARRAYS_PER_BITMAP_WORD as i32;
    let bit_index  = (array_index % ARRAYS_PER_BITMAP_WORD as i32) as u32;
    (word_index, bit_index)
}


#[error_code]
pub enum TickManagerError {
    #[msg("Tick index out of allowed range")]
    TickOutOfRange,
    #[msg("Tick is not a multiple of tick spacing")]
    InvalidTickSpacing,
    #[msg("TickArray is already initialized")]
    TickArrayAlreadyInitialized,
    #[msg("Tick is not initialized in this array")]
    TickNotInitialized,
    #[msg("Bitmap word index out of range")]
    BitmapOutOfRange,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_to_sqrt_price_tick_zero() {
        // tick 0 → √price = 1.0 → Q64 = 2^64
        let result = tick_to_sqrt_price_q64(0).unwrap();
        let expected: u128 = 1u128 << 64; 
        let diff = if result > expected { result - expected } else { expected - result };
        assert!(diff < 1_000_000, "tick 0 result={} expected={}", result, expected);
    }

    #[test]
    fn test_tick_to_sqrt_price_positive() {
       
        let result = tick_to_sqrt_price_q64(69082).unwrap();
        let expected: u128 = 583_140_621_595_701_381_120;
        let diff = if result > expected { result - expected } else { expected - result };
       
        assert!(diff < expected / 10_000, "tick 69082 result={} expected={}", result, expected);
    }

    #[test]
    fn test_tick_to_sqrt_price_negative() {
        
        let result = tick_to_sqrt_price_q64(-69082).unwrap();
        // Just verify it's less than Q64 (i.e., √price < 1)
        assert!(result < (1u128 << 64), "negative tick should give sqrt_price < 1");
        assert!(result > 0, "result should be positive");
    }

    #[test]
    fn test_array_start_tick() {
        let array_size = TICKS_PER_ARRAY as i32 * TICK_SPACING;
        assert_eq!(tick_to_array_start_tick(0), 0);
        assert_eq!(tick_to_array_start_tick(array_size - 1), 0);
        assert_eq!(tick_to_array_start_tick(array_size), array_size);
        assert_eq!(tick_to_array_start_tick(-1), -array_size);
        assert_eq!(tick_to_array_start_tick(-array_size), -array_size);
    }

    #[test]
    fn test_bitmap_word_and_bit() {
        assert_eq!(bitmap_word_and_bit(0), (0, 0));
        assert_eq!(bitmap_word_and_bit(7), (0, 7));
        assert_eq!(bitmap_word_and_bit(8), (1, 0));
        assert_eq!(bitmap_word_and_bit(10), (1, 2));
    }

    #[test]
    fn test_validate_tick_spacing() {
        assert!(validate_tick_spacing(0).is_ok());
        assert!(validate_tick_spacing(64).is_ok());
        assert!(validate_tick_spacing(128).is_ok());
        assert!(validate_tick_spacing(1).is_err());
        assert!(validate_tick_spacing(63).is_err());
    }
}
