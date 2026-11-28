//! Standalone target ABI and data layout computation for AE (`P6.1 / M5.1.01`).
//!
//! Computes deterministic sizes, alignments, and field offsets for AE types
//! across supported 64-bit targets without depending on the compiler crate.

use std::cmp::Reverse;
use std::fmt;

use ae_ty::{FloatTy, IntTy, TyKind, TypeInterner};

/// Supported compilation targets for ABI and layout computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetSpec {
    X86_64WindowsMsvc,
    X86_64LinuxGnu,
    Aarch64LinuxGnu,
}

impl TargetSpec {
    /// Returns the size of a pointer in bytes on this target.
    pub const fn pointer_size(self) -> u64 {
        match self {
            TargetSpec::X86_64WindowsMsvc
            | TargetSpec::X86_64LinuxGnu
            | TargetSpec::Aarch64LinuxGnu => 8,
        }
    }

    /// Returns the required alignment of a pointer in bytes on this target.
    pub const fn pointer_align(self) -> u64 {
        match self {
            TargetSpec::X86_64WindowsMsvc
            | TargetSpec::X86_64LinuxGnu
            | TargetSpec::Aarch64LinuxGnu => 8,
        }
    }

    /// Returns the size of C `long` in bytes on this target (LLP64 vs LP64).
    pub const fn c_long_size(self) -> u64 {
        match self {
            TargetSpec::X86_64WindowsMsvc => 4,
            TargetSpec::X86_64LinuxGnu | TargetSpec::Aarch64LinuxGnu => 8,
        }
    }

    /// Detects the host target specification at compile time.
    pub const fn host() -> Self {
        #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
        {
            TargetSpec::X86_64WindowsMsvc
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            TargetSpec::X86_64LinuxGnu
        }
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        {
            TargetSpec::Aarch64LinuxGnu
        }
        #[cfg(not(any(
            all(target_arch = "x86_64", target_os = "windows"),
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "aarch64", target_os = "linux"),
        )))]
        {
            if cfg!(target_os = "windows") {
                TargetSpec::X86_64WindowsMsvc
            } else {
                TargetSpec::X86_64LinuxGnu
            }
        }
    }
}

/// Errors returned when a layout cannot be computed cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// Alignment was zero or not a power of two.
    InvalidAlignment(u64),
    /// Size or offset calculation overflowed `u64`.
    SizeOverflow,
    /// Type does not have a standalone layout in `ae-layout`.
    UnsupportedType(&'static str),
    /// Nominal type (`Struct` or `Enum`) requires field definitions via `struct_layout` or `enum_layout`.
    UnresolvedNominalType(&'static str),
    /// An interner ID referenced by `TyKind` is out of bounds for the provided `TypeInterner`.
    InvalidTypeInternerId { kind: &'static str, id: usize },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::InvalidAlignment(align) => {
                write!(f, "invalid alignment {align}: must be a non-zero power of two")
            }
            LayoutError::SizeOverflow => write!(f, "layout size or offset overflowed u64"),
            LayoutError::UnsupportedType(name) => {
                write!(f, "type `{name}` does not have a standalone layout")
            }
            LayoutError::UnresolvedNominalType(name) => {
                write!(f, "nominal type `{name}` requires explicit field layouts")
            }
            LayoutError::InvalidTypeInternerId { kind, id } => {
                write!(f, "invalid {kind} interner id {id}")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// Size and alignment of a type in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Layout {
    pub size: u64,
    pub align: u64,
}

impl Layout {
    /// Zero-sized type layout (`size = 0, align = 1`).
    pub const ZERO: Self = Self { size: 0, align: 1 };

    /// Constructs a `Layout`, validating that `align` is a non-zero power of two.
    pub const fn new(size: u64, align: u64) -> Result<Self, LayoutError> {
        if align == 0 || !align.is_power_of_two() {
            return Err(LayoutError::InvalidAlignment(align));
        }
        Ok(Self { size, align })
    }

    /// Returns `true` if this layout occupies zero bytes.
    pub const fn is_zst(self) -> bool {
        self.size == 0
    }

    /// Returns the stride between consecutive elements of this layout in an array.
    pub const fn stride(self) -> Result<u64, LayoutError> {
        align_to(self.size, self.align)
    }
}

/// Rounds `value` up to the next multiple of `align`, checking alignment validity and overflow.
pub const fn align_to(value: u64, align: u64) -> Result<u64, LayoutError> {
    if align == 0 || !align.is_power_of_two() {
        return Err(LayoutError::InvalidAlignment(align));
    }
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(added) => Ok(added & !mask),
        None => Err(LayoutError::SizeOverflow),
    }
}

/// Layout of a struct, including per-field byte offsets and layouts in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructLayout {
    pub layout: Layout,
    pub field_offsets: Vec<u64>,
    pub field_layouts: Vec<Layout>,
}

/// Layout of a tagged enum matching AE's C runtime `struct { tag; union { struct { ... } v0; ... } u; }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumLayout {
    pub layout: Layout,
    pub tag_offset: u64,
    pub tag_size: u64,
    pub tag_align: u64,
    pub payload_offset: u64,
    pub variant_layouts: Vec<StructLayout>,
}

/// Computes the layout of a struct from its field layouts.
///
/// When `repr_c` is `true`, fields are placed in declaration order following C struct ABI rules.
/// When `repr_c` is `false`, fields are packed deterministically by descending alignment
/// (tie-breaking by descending size, then declaration index) while preserving declaration order
/// in `field_offsets` and `field_layouts`.
pub fn struct_layout(fields: &[Layout], repr_c: bool) -> Result<StructLayout, LayoutError> {
    for field in fields {
        if field.align == 0 || !field.align.is_power_of_two() {
            return Err(LayoutError::InvalidAlignment(field.align));
        }
    }

    let mut order: Vec<usize> = (0..fields.len()).collect();
    if !repr_c {
        order.sort_by_key(|&idx| (Reverse(fields[idx].align), Reverse(fields[idx].size), idx));
    }

    let mut field_offsets = vec![0u64; fields.len()];
    let mut offset = 0u64;
    let mut max_align = 1u64;

    for idx in order {
        let field = fields[idx];
        max_align = max_align.max(field.align);
        offset = align_to(offset, field.align)?;
        field_offsets[idx] = offset;
        offset = offset.checked_add(field.size).ok_or(LayoutError::SizeOverflow)?;
    }

    let total_size = align_to(offset, max_align)?;
    Ok(StructLayout {
        layout: Layout {
            size: total_size,
            align: max_align,
        },
        field_offsets,
        field_layouts: fields.to_vec(),
    })
}

/// Computes the layout of an enum matching AE's C runtime representation:
/// `typedef struct { ae_tag tag; union { ae_v0 v0; ... } u; } ae_enum;`.
///
/// If all variants are fieldless, the C union contains a `char ae_empty` placeholder
/// (`size = 1, align = 1`), matching `crates/ae-compiler` C output.
pub fn enum_layout(variants: &[&[Layout]], _target: TargetSpec) -> Result<EnumLayout, LayoutError> {
    // AE's C enum tag (`typedef enum { AE_E{id}_{v} = {v} }`) is a 32-bit C int on all supported targets.
    const TAG_OFFSET: u64 = 0;
    const TAG_SIZE: u64 = 4;
    const TAG_ALIGN: u64 = 4;

    let mut variant_layouts = Vec::with_capacity(variants.len());
    let mut has_payload_fields = false;
    let mut union_align = 1u64;
    let mut max_variant_size = 0u64;

    for fields in variants {
        let v_layout = struct_layout(fields, true)?;
        if !fields.is_empty() {
            has_payload_fields = true;
            union_align = union_align.max(v_layout.layout.align);
            max_variant_size = max_variant_size.max(v_layout.layout.size);
        }
        variant_layouts.push(v_layout);
    }

    let (union_size, union_align) = if has_payload_fields {
        (align_to(max_variant_size, union_align)?, union_align)
    } else {
        // Empty union gets `char ae_empty;` (size 1, align 1).
        (1, 1)
    };

    let payload_offset = align_to(TAG_SIZE, union_align)?;
    let total_align = TAG_ALIGN.max(union_align);
    let end = payload_offset
        .checked_add(union_size)
        .ok_or(LayoutError::SizeOverflow)?;
    let total_size = align_to(end, total_align)?;

    Ok(EnumLayout {
        layout: Layout {
            size: total_size,
            align: total_align,
        },
        tag_offset: TAG_OFFSET,
        tag_size: TAG_SIZE,
        tag_align: TAG_ALIGN,
        payload_offset,
        variant_layouts,
    })
}

/// Computes the layout of a bare C-like fieldless enum (just a 4-byte tag, no union wrapper).
pub const fn c_like_enum_layout(variant_count: usize, _target: TargetSpec) -> EnumLayout {
    let _ = variant_count;
    EnumLayout {
        layout: Layout { size: 4, align: 4 },
        tag_offset: 0,
        tag_size: 4,
        tag_align: 4,
        payload_offset: 4,
        variant_layouts: Vec::new(),
    }
}

/// Returns the `(ptr, len)` fat pointer layout for slices on `target`.
pub fn fat_pointer_layout(target: TargetSpec) -> StructLayout {
    let ptr_size = target.pointer_size();
    let ptr_align = target.pointer_align();
    let ptr_layout = Layout {
        size: ptr_size,
        align: ptr_align,
    };
    StructLayout {
        layout: Layout {
            size: ptr_size * 2,
            align: ptr_align,
        },
        field_offsets: vec![0, ptr_size],
        field_layouts: vec![ptr_layout, ptr_layout],
    }
}

/// Computes the layout of a fixed-size array `[T; len]` from the element layout.
pub const fn array_layout(elem: Layout, len: u64) -> Result<Layout, LayoutError> {
    let stride = match elem.stride() {
        Ok(s) => s,
        Err(e) => return Err(e),
    };
    let size = match stride.checked_mul(len) {
        Some(s) => s,
        None => return Err(LayoutError::SizeOverflow),
    };
    Ok(Layout {
        size,
        align: elem.align,
    })
}

/// Computes the deterministic layout of a concrete `TyKind` for `target`.
pub fn layout_of(
    ty: &TyKind,
    interner: &TypeInterner,
    target: TargetSpec,
) -> Result<Layout, LayoutError> {
    match ty {
        TyKind::Int(int_ty) => {
            let bytes = match int_ty {
                IntTy::I8 | IntTy::U8 => 1,
                IntTy::I16 | IntTy::U16 => 2,
                IntTy::I32 | IntTy::U32 => 4,
                IntTy::I64 | IntTy::U64 => 8,
                IntTy::Isize | IntTy::Usize => target.pointer_size(),
            };
            let align = match int_ty {
                IntTy::Isize | IntTy::Usize => target.pointer_align(),
                _ => bytes,
            };
            Ok(Layout { size: bytes, align })
        }
        TyKind::Float(float_ty) => {
            let bytes = match float_ty {
                FloatTy::F32 => 4,
                FloatTy::F64 => 8,
            };
            Ok(Layout {
                size: bytes,
                align: bytes,
            })
        }
        TyKind::Bool => Ok(Layout { size: 1, align: 1 }),
        TyKind::Char => Ok(Layout { size: 4, align: 4 }),
        TyKind::Unit => Ok(Layout::ZERO),
        TyKind::Ptr(_)
        | TyKind::PtrMut(_)
        | TyKind::StructRef(_)
        | TyKind::StructMut(_)
        | TyKind::VectorRef(_)
        | TyKind::MapRef(_)
        | TyKind::Thread(_)
        | TyKind::Fn(_) => Ok(Layout {
            size: target.pointer_size(),
            align: target.pointer_align(),
        }),
        TyKind::Slice(_) | TyKind::SliceMut(_) => Ok(fat_pointer_layout(target).layout),
        TyKind::ArrayOf(elem_id, len) => {
            if *elem_id >= interner.array_elem_count() {
                return Err(LayoutError::InvalidTypeInternerId {
                    kind: "array_elem",
                    id: *elem_id,
                });
            }
            let elem_ty = interner.array_elem(*elem_id);
            let elem_layout = layout_of(&elem_ty, interner, target)?;
            array_layout(elem_layout, *len as u64)
        }
        TyKind::Struct(_) => Err(LayoutError::UnresolvedNominalType("Struct")),
        TyKind::Enum(..) => Err(LayoutError::UnresolvedNominalType("Enum")),
        TyKind::String => Err(LayoutError::UnsupportedType("String")),
        TyKind::StrView => Err(LayoutError::UnsupportedType("StrView")),
        TyKind::Vector(_) => Err(LayoutError::UnsupportedType("Vector")),
        TyKind::MapOf(_) => Err(LayoutError::UnsupportedType("MapOf")),
        TyKind::Tokens => Err(LayoutError::UnsupportedType("Tokens")),
        TyKind::Closure(_) => Err(LayoutError::UnsupportedType("Closure")),
        TyKind::Atomic => Err(LayoutError::UnsupportedType("Atomic")),
        TyKind::Shared(_) => Err(LayoutError::UnsupportedType("Shared")),
        TyKind::Mutex => Err(LayoutError::UnsupportedType("Mutex")),
        TyKind::Guard => Err(LayoutError::UnsupportedType("Guard")),
        TyKind::Chan => Err(LayoutError::UnsupportedType("Chan")),
        TyKind::Exec => Err(LayoutError::UnsupportedType("Exec")),
        TyKind::TaskScope => Err(LayoutError::UnsupportedType("TaskScope")),
        TyKind::Pool => Err(LayoutError::UnsupportedType("Pool")),
    }
}
