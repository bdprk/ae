use ae_mir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, BorrowKind, CastKind,
    ConstFloat, ConstValue, Constant, LocalDecl, LocalId, LocalKind, MirValidationError,
    Mutability, Operand, Place, ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets,
    Terminator, TerminatorKind, UnOp, format_body, validate, validate_with_types,
};
use ae_span::SourceFile;
use ae_ty::{DefId, FloatTy, IntTy, TyKind, TypeInterner};

fn int_const(n: i128) -> Operand {
    Operand::Constant(Constant::new(TyKind::Int(IntTy::I64), ConstValue::Int(n)))
}

fn bool_const(b: bool) -> Operand {
    Operand::Constant(Constant::new(TyKind::Bool, ConstValue::Bool(b)))
}

#[test]
fn valid_arithmetic_cfg_and_deterministic_formatting() {
    let source = SourceFile {
        name: Some("math.ae"),
        text: "fn add_one(x: i64) -> i64 { x + 1 }",
    };
    let span = source.span(0..35).unwrap();

    let mut body = Body::new("add_one", 1);
    body.def = Some(DefId(7));
    body.span = Some(span.clone());
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Not, LocalKind::Arg).with_name("x"),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            Statement::new(StatementKind::StorageLive(LocalId(2))),
            Statement::with_span(
                StatementKind::Assign(
                    Place::from_local(LocalId(2)),
                    Rvalue::Use(int_const(1)),
                ),
                span,
            ),
            Statement::new(StatementKind::Assign(
                Place::from_local(LocalId::RETURN_PLACE),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(1))),
                    Operand::Move(Place::from_local(LocalId(2))),
                ),
            )),
            Statement::new(StatementKind::StorageDead(LocalId(2))),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(validate(&body), Ok(()));

    let formatted = format_body(&body);
    assert_eq!(formatted, body.to_string());
    let expected = "\
fn add_one(_1: i64) -> i64 {
    let mut _0: i64;
    let _1: i64; // x
    let mut _2: i64;

    bb0: {
        StorageLive(_2);
        _2 = const 1_i64;
        _0 = Add(copy _1, move _2);
        StorageDead(_2);
        return;
    }
}";
    assert_eq!(formatted, expected);
}

#[test]
fn valid_short_circuit_switch_int_body() {
    // Models: fn and_bool(a: bool, b: bool) -> bool { a && b }
    let mut body = Body::new("and_bool", 2);
    body.locals = vec![
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg).with_name("a"),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg).with_name("b"),
    ];
    body.basic_blocks = vec![
        // bb0: if a -> bb1 else -> bb2
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(1))),
                targets: SwitchTargets::if_bool(BasicBlockId(1), BasicBlockId(2)),
            })),
        ),
        // bb1: _0 = b; goto bb3
        BasicBlockData::new(
            vec![Statement::new(StatementKind::Assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(2)))),
            ))],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        // bb2: _0 = false; goto bb3
        BasicBlockData::new(
            vec![Statement::new(StatementKind::Assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(bool_const(false)),
            ))],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        // bb3: return
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
    ];

    assert_eq!(validate(&body), Ok(()));
    let text = format_body(&body);
    assert!(text.contains("switchInt(copy _1) -> [0: bb2, otherwise: bb1];"));
}

#[test]
fn valid_drop_call_assert_and_projections_with_interner() {
    let mut interner = TypeInterner::new();
    let ptr_i64 = interner.intern_pointee(TyKind::Int(IntTy::I64));
    let arr_i64 = interner.intern_array_elem(TyKind::Int(IntTy::I64));

    let mut body = Body::new("drop_and_proj", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Var).with_name("s"),
        LocalDecl::new(TyKind::PtrMut(ptr_i64), Mutability::Not, LocalKind::Temp),
        LocalDecl::new(TyKind::ArrayOf(arr_i64, 3), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Not, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Float(FloatTy::F64), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(1)),
                    Rvalue::Use(Operand::Constant(Constant::new(
                        TyKind::String,
                        ConstValue::String("hi".to_owned()),
                    ))),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(4)),
                    Rvalue::Use(int_const(1)),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(3)),
                    Rvalue::Aggregate(
                        AggregateKind::Array(TyKind::Int(IntTy::I64), 0),
                        vec![int_const(10), int_const(20), int_const(30)],
                    ),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(5)),
                    Rvalue::Use(Operand::Copy(
                        Place::from_local(LocalId(3)).project(ProjectionElem::Index(LocalId(4))),
                    )),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                    Rvalue::UnaryOp(UnOp::Neg, Operand::Copy(Place::from_local(LocalId(5)))),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(6)),
                    Rvalue::Cast(
                        CastKind::IntToFloat,
                        Operand::Copy(Place::from_local(LocalId(5))),
                        TyKind::Float(FloatTy::F64),
                    ),
                )),
                Statement::new(StatementKind::Assign(
                    Place::from_local(LocalId(6)),
                    Rvalue::Use(Operand::Constant(Constant::new(
                        TyKind::Float(FloatTy::F64),
                        ConstValue::Float(ConstFloat::from_f64(2.5)),
                    ))),
                )),
                Statement::new(StatementKind::Nop),
            ],
            Some(Terminator::new(TerminatorKind::Assert {
                cond: bool_const(true),
                expected: true,
                msg: AssertMessage::BoundsCheck {
                    len: int_const(3),
                    index: Operand::Copy(Place::from_local(LocalId(4))),
                },
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(
            vec![Statement::new(StatementKind::Assign(
                Place::from_local(LocalId(0)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1)),
                },
            ))],
            Some(Terminator::new(TerminatorKind::Drop {
                place: Place::from_local(LocalId(1)),
                target: BasicBlockId(2),
                unwind: None,
            })),
        ),
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Call {
                func: Operand::Constant(Constant::new(TyKind::Fn(0), ConstValue::FnDef(DefId(1)))),
                args: vec![],
                destination: Place::from_local(LocalId(0)),
                target: Some(BasicBlockId(3)),
            })),
        ),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
    ];

    assert_eq!(validate_with_types(&body, Some(&interner)), Ok(()));
}

#[test]
fn negative_empty_body() {
    let body = Body::new("empty", 0);
    assert_eq!(validate(&body), Err(vec![MirValidationError::EmptyBody]));
}

#[test]
fn negative_invalid_successor_and_missing_terminator() {
    let mut body = Body::new("bad_cfg", 0);
    body.locals = vec![LocalDecl::new(
        TyKind::Unit,
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(5),
            })),
        ),
        BasicBlockData::new(vec![], None),
    ];

    let errs = validate(&body).unwrap_err();
    assert!(errs.contains(&MirValidationError::InvalidBasicBlock {
        from: BasicBlockId(0),
        target: BasicBlockId(5),
    }));
    assert!(errs.contains(&MirValidationError::MissingTerminator {
        block: BasicBlockId(1),
    }));
    assert!(errs.contains(&MirValidationError::UnreachableBlock {
        block: BasicBlockId(1),
    }));
}

#[test]
fn negative_out_of_range_local() {
    let mut body = Body::new("bad_local", 0);
    body.locals = vec![LocalDecl::new(
        TyKind::Int(IntTy::I64),
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![Statement::new(StatementKind::Assign(
            Place::from_local(LocalId(0)),
            Rvalue::Use(Operand::Copy(Place::from_local(LocalId(9)))),
        ))],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = validate(&body).unwrap_err();
    assert!(errs.contains(&MirValidationError::InvalidLocal {
        block: BasicBlockId(0),
        local: LocalId(9),
    }));
}

#[test]
fn negative_bad_assignment_and_bad_drop() {
    let mut body = Body::new("bad_types", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            // Assigning bool to i64
            Statement::new(StatementKind::Assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(bool_const(true)),
            )),
            // Dropping non-owned bool
            Statement::new(StatementKind::Drop(Place::from_local(LocalId(1)))),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = validate(&body).unwrap_err();
    assert!(errs.contains(&MirValidationError::TypeMismatch {
        block: BasicBlockId(0),
        expected: TyKind::Int(IntTy::I64),
        found: TyKind::Bool,
    }));
    assert!(errs.contains(&MirValidationError::InvalidDrop {
        block: BasicBlockId(0),
        ty: TyKind::Bool,
    }));
}

#[test]
fn negative_bad_projections_and_switch_discriminant() {
    let mut body = Body::new("bad_proj", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![Statement::new(StatementKind::Assign(
            Place::from_local(LocalId(0)).project(ProjectionElem::Deref),
            Rvalue::Use(int_const(1)),
        ))],
        Some(Terminator::new(TerminatorKind::SwitchInt {
            discr: Operand::Move(Place::from_local(LocalId(1))),
            targets: SwitchTargets::if_bool(BasicBlockId(0), BasicBlockId(0)),
        })),
    )];

    let errs = validate(&body).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        MirValidationError::InvalidProjection {
            reason: "cannot dereference non-pointer type",
            ..
        }
    )));
    assert!(errs.contains(&MirValidationError::InvalidSwitchDiscriminant {
        block: BasicBlockId(0),
        ty: TyKind::String,
    }));
}
