use std::fs;
use std::process::Command;

use ae_layout::{
    Layout, LayoutError, TargetSpec, align_to, array_layout, c_like_enum_layout, enum_layout,
    fat_pointer_layout, layout_of, struct_layout,
};
use ae_ty::{FloatTy, IntTy, TyKind, TypeInterner};

const ALL_TARGETS: [TargetSpec; 3] = [
    TargetSpec::X86_64WindowsMsvc,
    TargetSpec::X86_64LinuxGnu,
    TargetSpec::Aarch64LinuxGnu,
];

#[test]
fn target_spec_properties() {
    for target in ALL_TARGETS {
        assert_eq!(target.pointer_size(), 8);
        assert_eq!(target.pointer_align(), 8);
    }
    assert_eq!(TargetSpec::X86_64WindowsMsvc.c_long_size(), 4);
    assert_eq!(TargetSpec::X86_64LinuxGnu.c_long_size(), 8);
    assert_eq!(TargetSpec::Aarch64LinuxGnu.c_long_size(), 8);

    let host = TargetSpec::host();
    assert!(ALL_TARGETS.contains(&host));
}

#[test]
fn scalar_and_pointer_layouts_across_all_targets() {
    let mut interner = TypeInterner::new();
    let ptr_id = interner.intern_pointee(TyKind::Int(IntTy::I32));
    let slice_id = interner.intern_slice_elem(TyKind::Int(IntTy::U8));
    let arr_u8_id = interner.intern_array_elem(TyKind::Int(IntTy::U8));
    let arr_i32_id = interner.intern_array_elem(TyKind::Int(IntTy::I32));

    let expected_cases = [
        (TyKind::Int(IntTy::I8), Layout { size: 1, align: 1 }),
        (TyKind::Int(IntTy::I16), Layout { size: 2, align: 2 }),
        (TyKind::Int(IntTy::I32), Layout { size: 4, align: 4 }),
        (TyKind::Int(IntTy::I64), Layout { size: 8, align: 8 }),
        (TyKind::Int(IntTy::Isize), Layout { size: 8, align: 8 }),
        (TyKind::Int(IntTy::U8), Layout { size: 1, align: 1 }),
        (TyKind::Int(IntTy::U16), Layout { size: 2, align: 2 }),
        (TyKind::Int(IntTy::U32), Layout { size: 4, align: 4 }),
        (TyKind::Int(IntTy::U64), Layout { size: 8, align: 8 }),
        (TyKind::Int(IntTy::Usize), Layout { size: 8, align: 8 }),
        (TyKind::Float(FloatTy::F32), Layout { size: 4, align: 4 }),
        (TyKind::Float(FloatTy::F64), Layout { size: 8, align: 8 }),
        (TyKind::Bool, Layout { size: 1, align: 1 }),
        (TyKind::Char, Layout { size: 4, align: 4 }),
        (TyKind::Unit, Layout { size: 0, align: 1 }),
        (TyKind::Ptr(ptr_id), Layout { size: 8, align: 8 }),
        (TyKind::PtrMut(ptr_id), Layout { size: 8, align: 8 }),
        (TyKind::StructRef(0), Layout { size: 8, align: 8 }),
        (TyKind::StructMut(0), Layout { size: 8, align: 8 }),
        (TyKind::VectorRef(0), Layout { size: 8, align: 8 }),
        (TyKind::Thread(0), Layout { size: 8, align: 8 }),
        (TyKind::Fn(0), Layout { size: 8, align: 8 }),
        (TyKind::Slice(slice_id), Layout { size: 16, align: 8 }),
        (TyKind::SliceMut(slice_id), Layout { size: 16, align: 8 }),
        (TyKind::ArrayOf(arr_u8_id, 0), Layout { size: 0, align: 1 }),
        (TyKind::ArrayOf(arr_u8_id, 7), Layout { size: 7, align: 1 }),
        (TyKind::ArrayOf(arr_i32_id, 5), Layout { size: 20, align: 4 }),
    ];

    for target in ALL_TARGETS {
        for (ty, expected) in expected_cases {
            let actual = layout_of(&ty, &interner, target).expect("scalar/array/ptr layout");
            assert_eq!(actual, expected, "mismatch for {ty:?} on {target:?}");
        }

        let fat = fat_pointer_layout(target);
        assert_eq!(fat.layout, Layout { size: 16, align: 8 });
        assert_eq!(fat.field_offsets, vec![0, 8]);
        assert_eq!(
            fat.field_layouts,
            vec![
                Layout { size: 8, align: 8 },
                Layout { size: 8, align: 8 },
            ]
        );
    }

    assert!(Layout::ZERO.is_zst());
    assert!(!Layout { size: 1, align: 1 }.is_zst());
}

#[test]
fn repr_c_and_packed_structs_and_nested_structs() {
    let interner = TypeInterner::new();
    for target in ALL_TARGETS {
        let u8_l = layout_of(&TyKind::Int(IntTy::U8), &interner, target).unwrap();
        let i64_l = layout_of(&TyKind::Int(IntTy::I64), &interner, target).unwrap();
        let u16_l = layout_of(&TyKind::Int(IntTy::U16), &interner, target).unwrap();
        let i32_l = layout_of(&TyKind::Int(IntTy::I32), &interner, target).unwrap();

        // Fields: (u8, i64, u16, u8)
        // repr(C): u8@0, pad 7, i64@8, u16@16, u8@18, pad 5 => size 24, align 8
        let c_struct = struct_layout(&[u8_l, i64_l, u16_l, u8_l], true).unwrap();
        assert_eq!(c_struct.layout, Layout { size: 24, align: 8 });
        assert_eq!(c_struct.field_offsets, vec![0, 8, 16, 18]);
        assert_eq!(c_struct.field_layouts, vec![u8_l, i64_l, u16_l, u8_l]);

        // default deterministic packing: i64@0, u16@8, u8@10, u8@11, pad 4 => size 16, align 8
        let packed = struct_layout(&[u8_l, i64_l, u16_l, u8_l], false).unwrap();
        assert_eq!(packed.layout, Layout { size: 16, align: 8 });
        assert_eq!(packed.field_offsets, vec![10, 0, 8, 11]);
        assert_eq!(packed.field_layouts, vec![u8_l, i64_l, u16_l, u8_l]);

        // Nested struct: { u8, c_struct, i32 }
        // u8@0, pad 7, c_struct@8..32, i32@32..36, pad 4 => size 40, align 8
        let nested = struct_layout(&[u8_l, c_struct.layout, i32_l], true).unwrap();
        assert_eq!(nested.layout, Layout { size: 40, align: 8 });
        assert_eq!(nested.field_offsets, vec![0, 8, 32]);

        // Empty struct is ZST
        let empty = struct_layout(&[], true).unwrap();
        assert_eq!(empty.layout, Layout::ZERO);
        assert!(empty.field_offsets.is_empty());
    }
}

#[test]
fn enum_layouts_across_all_targets() {
    let interner = TypeInterner::new();
    for target in ALL_TARGETS {
        let u8_l = layout_of(&TyKind::Int(IntTy::U8), &interner, target).unwrap();
        let i32_l = layout_of(&TyKind::Int(IntTy::I32), &interner, target).unwrap();
        let i64_l = layout_of(&TyKind::Int(IntTy::I64), &interner, target).unwrap();

        // Bare C-like enum
        let bare = c_like_enum_layout(3, target);
        assert_eq!(bare.layout, Layout { size: 4, align: 4 });
        assert_eq!(bare.tag_offset, 0);
        assert_eq!(bare.tag_size, 4);
        assert_eq!(bare.tag_align, 4);

        // Fieldless AE C-runtime enum: struct { tag; union { char ae_empty; } u; }
        let fieldless = enum_layout(&[&[], &[], &[]], target).unwrap();
        assert_eq!(fieldless.layout, Layout { size: 8, align: 4 });
        assert_eq!(fieldless.tag_offset, 0);
        assert_eq!(fieldless.tag_size, 4);
        assert_eq!(fieldless.tag_align, 4);
        assert_eq!(fieldless.payload_offset, 4);
        assert_eq!(fieldless.variant_layouts.len(), 3);

        // Tagged payload enum: v0(i32, u8) [size 8, align 4], v1(), v2(i64, u8) [size 16, align 8]
        // tag@0 (4,4), union_align = 8 => payload_offset = 8, union_size = 16 => total size = 24, align = 8
        let tagged = enum_layout(&[&[i32_l, u8_l], &[], &[i64_l, u8_l]], target).unwrap();
        assert_eq!(tagged.layout, Layout { size: 24, align: 8 });
        assert_eq!(tagged.tag_offset, 0);
        assert_eq!(tagged.payload_offset, 8);
        assert_eq!(tagged.variant_layouts[0].layout, Layout { size: 8, align: 4 });
        assert_eq!(tagged.variant_layouts[0].field_offsets, vec![0, 4]);
        assert_eq!(tagged.variant_layouts[1].layout, Layout::ZERO);
        assert_eq!(tagged.variant_layouts[2].layout, Layout { size: 16, align: 8 });
        assert_eq!(tagged.variant_layouts[2].field_offsets, vec![0, 8]);
    }
}

#[test]
fn errors_and_overflow_never_panic() {
    let interner = TypeInterner::new();
    let target = TargetSpec::host();

    assert_eq!(align_to(10, 0), Err(LayoutError::InvalidAlignment(0)));
    assert_eq!(align_to(10, 3), Err(LayoutError::InvalidAlignment(3)));
    assert_eq!(align_to(u64::MAX, 8), Err(LayoutError::SizeOverflow));
    assert_eq!(Layout::new(8, 3), Err(LayoutError::InvalidAlignment(3)));

    let big = Layout::new(u64::MAX / 2 + 1, 8).unwrap();
    assert_eq!(array_layout(big, 2), Err(LayoutError::SizeOverflow));
    assert_eq!(
        struct_layout(&[big, big], true),
        Err(LayoutError::SizeOverflow)
    );

    assert_eq!(
        layout_of(&TyKind::ArrayOf(99, 4), &interner, target),
        Err(LayoutError::InvalidTypeInternerId {
            kind: "array_elem",
            id: 99,
        })
    );
    assert_eq!(
        layout_of(&TyKind::Struct(0), &interner, target),
        Err(LayoutError::UnresolvedNominalType("Struct"))
    );
    assert_eq!(
        layout_of(&TyKind::Enum("E", 0), &interner, target),
        Err(LayoutError::UnresolvedNominalType("Enum"))
    );
    assert_eq!(
        layout_of(&TyKind::String, &interner, target),
        Err(LayoutError::UnsupportedType("String"))
    );
}

fn find_c_compiler() -> String {
    if let Ok(cc) = std::env::var("CC")
        && !cc.trim().is_empty()
    {
        return cc;
    }
    for candidate in ["gcc", "cc", "clang"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return candidate.to_string();
        }
    }
    panic!("no C compiler found (checked CC env var, gcc, cc, clang)");
}

#[test]
fn c_abi_probe_matches_host_compiler() {
    let cc = find_c_compiler();
    let target = TargetSpec::host();
    let mut interner = TypeInterner::new();

    let u8_l = layout_of(&TyKind::Int(IntTy::U8), &interner, target).unwrap();
    let u16_l = layout_of(&TyKind::Int(IntTy::U16), &interner, target).unwrap();
    let i32_l = layout_of(&TyKind::Int(IntTy::I32), &interner, target).unwrap();
    let i64_l = layout_of(&TyKind::Int(IntTy::I64), &interner, target).unwrap();

    // Mixed struct: { uint8_t a; int64_t b; uint16_t c; uint8_t d; }
    let mixed = struct_layout(&[u8_l, i64_l, u16_l, u8_l], true).unwrap();
    // Nested struct: { uint8_t tag; struct Mixed inner; int32_t tail; }
    let nested = struct_layout(&[u8_l, mixed.layout, i32_l], true).unwrap();
    // Array of 3 Mixed structs
    let arr_mixed = array_layout(mixed.layout, 3).unwrap();
    // Array of 5 i32
    let arr_i32_id = interner.intern_array_elem(TyKind::Int(IntTy::I32));
    let arr_i32 = layout_of(&TyKind::ArrayOf(arr_i32_id, 5), &interner, target).unwrap();
    // Fat pointer (slice)
    let fat = fat_pointer_layout(target);
    // Fieldless enum (AE C runtime style)
    let e_fieldless = enum_layout(&[&[], &[]], target).unwrap();
    // Tagged payload enum: v0(int32_t, uint8_t), v1(), v2(struct Mixed)
    let e_tagged = enum_layout(&[&[i32_l, u8_l], &[], &[mixed.layout]], target).unwrap();

    let c_src = r#"
#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

typedef struct {
    const void *ptr;
    uintptr_t len;
} AeSlice;

typedef struct {
    uint8_t a;
    int64_t b;
    uint16_t c;
    uint8_t d;
} Mixed;

typedef struct {
    uint8_t tag;
    Mixed inner;
    int32_t tail;
} Nested;

typedef enum { E0_A = 0, E0_B = 1 } e0_tag;
typedef struct {
    e0_tag tag;
    union {
        char ae_empty;
    } u;
} EFieldless;

typedef enum { E1_0 = 0, E1_1 = 1, E1_2 = 2 } e1_tag;
typedef struct {
    int32_t p0;
    uint8_t p1;
} e1_v0;
typedef struct {
    Mixed p0;
} e1_v2;
typedef struct {
    e1_tag tag;
    union {
        e1_v0 v0;
        e1_v2 v2;
    } u;
} ETagged;

int main(void) {
    printf("i8=%zu,%zu\n", sizeof(int8_t), _Alignof(int8_t));
    printf("i16=%zu,%zu\n", sizeof(int16_t), _Alignof(int16_t));
    printf("i32=%zu,%zu\n", sizeof(int32_t), _Alignof(int32_t));
    printf("i64=%zu,%zu\n", sizeof(int64_t), _Alignof(int64_t));
    printf("isize=%zu,%zu\n", sizeof(intptr_t), _Alignof(intptr_t));
    printf("f32=%zu,%zu\n", sizeof(float), _Alignof(float));
    printf("f64=%zu,%zu\n", sizeof(double), _Alignof(double));
    printf("bool=%zu,%zu\n", sizeof(bool), _Alignof(bool));
    printf("char=%zu,%zu\n", sizeof(uint32_t), _Alignof(uint32_t));
    printf("ptr=%zu,%zu\n", sizeof(void *), _Alignof(void *));
    printf("fat=%zu,%zu,%zu,%zu\n", sizeof(AeSlice), _Alignof(AeSlice), offsetof(AeSlice, ptr), offsetof(AeSlice, len));
    printf("arr_i32=%zu,%zu\n", sizeof(int32_t[5]), _Alignof(int32_t[5]));
    printf("arr_mixed=%zu,%zu\n", sizeof(Mixed[3]), _Alignof(Mixed[3]));
    printf("mixed=%zu,%zu,%zu,%zu,%zu,%zu\n", sizeof(Mixed), _Alignof(Mixed), offsetof(Mixed, a), offsetof(Mixed, b), offsetof(Mixed, c), offsetof(Mixed, d));
    printf("nested=%zu,%zu,%zu,%zu,%zu\n", sizeof(Nested), _Alignof(Nested), offsetof(Nested, tag), offsetof(Nested, inner), offsetof(Nested, tail));
    printf("e_fieldless=%zu,%zu,%zu,%zu\n", sizeof(EFieldless), _Alignof(EFieldless), offsetof(EFieldless, tag), offsetof(EFieldless, u));
    printf("e_tagged=%zu,%zu,%zu,%zu,%zu,%zu\n",
        sizeof(ETagged), _Alignof(ETagged),
        offsetof(ETagged, tag), offsetof(ETagged, u),
        offsetof(e1_v0, p0), offsetof(e1_v0, p1));
    return 0;
}
"#;

    let mut c_path = std::env::temp_dir();
    let pid = std::process::id();
    c_path.push(format!("ae_layout_probe_{pid}.c"));
    let mut exe_path = std::env::temp_dir();
    exe_path.push(if cfg!(windows) {
        format!("ae_layout_probe_{pid}.exe")
    } else {
        format!("ae_layout_probe_{pid}")
    });

    fs::write(&c_path, c_src).expect("write probe C source");
    let compile = Command::new(&cc)
        .arg("-std=c11")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("invoke C compiler");
    assert!(
        compile.status.success(),
        "C probe compile failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe_path).output().expect("run C probe");
    let _ = fs::remove_file(&c_path);
    let _ = fs::remove_file(&exe_path);
    assert!(run.status.success(), "C probe exited with failure");

    let stdout = String::from_utf8(run.stdout).expect("valid utf8");
    let parse_nums = |key: &str| -> Vec<u64> {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("missing key `{key}` in C output:\n{stdout}"))
            .split(',')
            .map(|n| n.trim().parse::<u64>().unwrap())
            .collect()
    };

    let check_layout = |key: &str, expected: Layout| {
        let vals = parse_nums(key);
        assert_eq!(vals, vec![expected.size, expected.align], "mismatch for {key}");
    };

    check_layout("i8", layout_of(&TyKind::Int(IntTy::I8), &interner, target).unwrap());
    check_layout("i16", layout_of(&TyKind::Int(IntTy::I16), &interner, target).unwrap());
    check_layout("i32", i32_l);
    check_layout("i64", i64_l);
    check_layout("isize", layout_of(&TyKind::Int(IntTy::Isize), &interner, target).unwrap());
    check_layout("f32", layout_of(&TyKind::Float(FloatTy::F32), &interner, target).unwrap());
    check_layout("f64", layout_of(&TyKind::Float(FloatTy::F64), &interner, target).unwrap());
    check_layout("bool", layout_of(&TyKind::Bool, &interner, target).unwrap());
    check_layout("char", layout_of(&TyKind::Char, &interner, target).unwrap());
    check_layout("ptr", layout_of(&TyKind::Ptr(0), &interner, target).unwrap());
    check_layout("arr_i32", arr_i32);
    check_layout("arr_mixed", arr_mixed);

    assert_eq!(
        parse_nums("fat"),
        vec![fat.layout.size, fat.layout.align, fat.field_offsets[0], fat.field_offsets[1]]
    );
    assert_eq!(
        parse_nums("mixed"),
        vec![
            mixed.layout.size,
            mixed.layout.align,
            mixed.field_offsets[0],
            mixed.field_offsets[1],
            mixed.field_offsets[2],
            mixed.field_offsets[3],
        ]
    );
    assert_eq!(
        parse_nums("nested"),
        vec![
            nested.layout.size,
            nested.layout.align,
            nested.field_offsets[0],
            nested.field_offsets[1],
            nested.field_offsets[2],
        ]
    );
    assert_eq!(
        parse_nums("e_fieldless"),
        vec![
            e_fieldless.layout.size,
            e_fieldless.layout.align,
            e_fieldless.tag_offset,
            e_fieldless.payload_offset,
        ]
    );
    assert_eq!(
        parse_nums("e_tagged"),
        vec![
            e_tagged.layout.size,
            e_tagged.layout.align,
            e_tagged.tag_offset,
            e_tagged.payload_offset,
            e_tagged.variant_layouts[0].field_offsets[0],
            e_tagged.variant_layouts[0].field_offsets[1],
        ]
    );
}
