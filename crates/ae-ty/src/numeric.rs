//! Fixed-width numeric vocabulary (P2.1, ADR-0232).
//!
//! The compiler's concrete type will carry these tags (`TyKind::Int(IntTy)`,
//! `TyKind::Float(FloatTy)`); the tables live here so the frontend, MIR, C
//! backend and interpreter agree on names, widths and signedness. `isize` and
//! `usize` are the pointer width; both supported targets are 64-bit, so the
//! table reports 64 bits and a future 32-bit target changes one place.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FloatTy {
    F32,
    F64,
}

impl IntTy {
    /// The width an unsuffixed integer literal has.
    pub const DEFAULT: IntTy = IntTy::I64;

    pub fn signed(self) -> bool {
        matches!(self, IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize)
    }

    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::Isize => "isize",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
            IntTy::Usize => "usize",
        }
    }

    pub fn c_name(self) -> &'static str {
        match self {
            IntTy::I8 => "int8_t",
            IntTy::I16 => "int16_t",
            IntTy::I32 => "int32_t",
            IntTy::I64 => "int64_t",
            IntTy::Isize => "intptr_t",
            IntTy::U8 => "uint8_t",
            IntTy::U16 => "uint16_t",
            IntTy::U32 => "uint32_t",
            IntTy::U64 => "uint64_t",
            IntTy::Usize => "uintptr_t",
        }
    }

    /// The type's value range, inclusive. Unsigned maxima do not fit `i128`
    /// for a 128-bit width, but this table stops at 64 bits.
    pub fn range(self) -> (i128, i128) {
        if self.signed() {
            let bits = self.bits();
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (0, (1i128 << self.bits()) - 1)
        }
    }
}

impl FloatTy {
    /// The width an unsuffixed floating literal has.
    pub const DEFAULT: FloatTy = FloatTy::F64;

    pub fn bits(self) -> u32 {
        match self {
            FloatTy::F32 => 32,
            FloatTy::F64 => 64,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
        }
    }

    pub fn c_name(self) -> &'static str {
        match self {
            FloatTy::F32 => "float",
            FloatTy::F64 => "double",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_names_and_signedness_are_consistent() {
        assert_eq!(IntTy::I8.c_name(), "int8_t");
        assert_eq!(IntTy::Usize.c_name(), "uintptr_t");
        assert_eq!(IntTy::I64.name(), "i64");
        assert_eq!(IntTy::Isize.bits(), 64);
        assert!(IntTy::Isize.signed() && !IntTy::U64.signed());
        assert_eq!(IntTy::DEFAULT, IntTy::I64);
        assert_eq!(FloatTy::F32.c_name(), "float");
        assert_eq!(FloatTy::DEFAULT, FloatTy::F64);
    }

    #[test]
    fn ranges_are_inclusive_and_width_correct() {
        assert_eq!(IntTy::I8.range(), (-128, 127));
        assert_eq!(IntTy::U8.range(), (0, 255));
        assert_eq!(IntTy::I16.range(), (-32768, 32767));
        assert_eq!(IntTy::U16.range(), (0, 65535));
        assert_eq!(IntTy::I32.range(), (-2147483648, 2147483647));
        assert_eq!(IntTy::U32.range(), (0, 4294967295));
        assert_eq!(IntTy::I64.range(), (i64::MIN as i128, i64::MAX as i128));
        assert_eq!(IntTy::U64.range(), (0, u64::MAX as i128));
    }
}