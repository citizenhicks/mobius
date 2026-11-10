//! Numeric conversions for layout, animation and display math, kept in one place. Each
//! one behaves exactly like an `as` cast: integers round to the nearest float, floats
//! truncate toward zero and saturate at the target's bounds, and NaN becomes zero.

/// Integer and wide-float values converted for float math.
pub trait ToFloat: Copy {
    /// The nearest `f32`.
    fn to_f32(self) -> f32;
    /// The nearest `f64`.
    fn to_f64(self) -> f64;
}

macro_rules! to_float {
    ($($source:ty),*) => {$(
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts, sizes and times in layout and display math stay far below 2^24"
        )]
        impl ToFloat for $source {
            fn to_f32(self) -> f32 {
                self as f32
            }

            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    )*};
}

to_float!(usize, u32, u64, u128, i32, i64);

impl ToFloat for f64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "rounding to the nearest f32 is the narrowing wanted for drawing"
    )]
    fn to_f32(self) -> f32 {
        self as f32
    }

    fn to_f64(self) -> f64 {
        self
    }
}

/// Floats converted to integers by truncating toward zero and saturating at the bounds.
pub trait Saturating: Copy {
    /// Saturating `u8`.
    fn saturating_u8(self) -> u8;
    /// Saturating `u32`.
    fn saturating_u32(self) -> u32;
    /// Saturating `i32`.
    fn saturating_i32(self) -> i32;
    /// Saturating `i64`.
    fn saturating_i64(self) -> i64;
    /// Saturating `usize`.
    fn saturating_usize(self) -> usize;
}

macro_rules! saturating {
    ($($source:ty),*) => {$(
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "float-to-integer `as` truncates and saturates, which is the conversion wanted"
        )]
        impl Saturating for $source {
            fn saturating_u8(self) -> u8 {
                self as u8
            }

            fn saturating_u32(self) -> u32 {
                self as u32
            }

            fn saturating_i32(self) -> i32 {
                self as i32
            }

            fn saturating_i64(self) -> i64 {
                self as i64
            }

            fn saturating_usize(self) -> usize {
                self as usize
            }
        }
    )*};
}

saturating!(f32, f64);
