use ae_mir::{
    BasicBlockData, BasicBlockId, Body, BorrowErrorKind, BorrowKind, ConstValue, Constant, LocalDecl, LocalId, LocalKind, Mutability, Operand, Place, ProjectionElem, Rvalue,
    Statement, StatementKind, SwitchTargets, Terminator, TerminatorKind, check_body,
};
use ae_ty::{IntTy, TyKind};

fn i64_ty() -> TyKind {
    TyKind::Int(IntTy::I64)
}

fn const_i64(v: i128) -> Operand {
    Operand::Constant(Constant::new(i64_ty(), ConstValue::Int(v)))
}

fn assign(place: Place, rvalue: Rvalue) -> Statement {
    Statement::new(StatementKind::Assign(place, rvalue))
}

fn field_place(local: LocalId, field: usize) -> Place {
    Place::from_local(local).project(ProjectionElem::Field(field))
}

fn index_place(local: LocalId, idx_local: LocalId) -> Place {
    Place::from_local(local).project(ProjectionElem::Index(idx_local))
}

#[test]
fn partial_move_allows_disjoint_field_read() {
    // _0: String, _1: Struct(0) (arg with 2 non-Copy fields), _2: String
    let mut body = Body::new("partial_move_ok", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Use(Operand::Move(field_place(LocalId(1), 0))),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(field_place(LocalId(1), 1))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(check_body(&body), Ok(()));
}

#[test]
fn moved_field_read_is_rejected() {
    let mut body = Body::new("moved_field_err", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Use(Operand::Move(field_place(LocalId(1), 0))),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(field_place(LocalId(1), 0))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected use-after-move error");
    assert!(
        errs.iter().any(|e| matches!(
            &e.kind,
            BorrowErrorKind::UseOfMoved { place } if *place == field_place(LocalId(1), 0)
        )),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn whole_struct_read_after_partial_move_is_rejected() {
    let mut body = Body::new("whole_after_partial_move_err", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Use(Operand::Move(field_place(LocalId(1), 0))),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(1)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected use of partially moved struct");
    assert!(
        errs.iter().any(|e| matches!(
            &e.kind,
            BorrowErrorKind::UseOfMaybeUninitialized { place } if *place == Place::from_local(LocalId(1))
        )),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn reinitializing_moved_field_restores_whole_struct() {
    let mut body = Body::new("reinit_field_ok", 2);
    body.locals = vec![
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(3)),
                Rvalue::Use(Operand::Move(field_place(LocalId(1), 0))),
            ),
            assign(
                field_place(LocalId(1), 0),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(1)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(check_body(&body), Ok(()));
}

#[test]
fn uninitialized_local_read_is_rejected() {
    let mut body = Body::new("uninit_err", 0);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            Place::from_local(LocalId(0)),
            Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected uninitialized local error");
    assert!(
        errs.iter().any(|e| matches!(
            &e.kind,
            BorrowErrorKind::UseOfUninitialized { place } if *place == Place::from_local(LocalId(1))
        )),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn branch_merge_use_after_move_on_one_branch_is_rejected() {
    // bb0: switch(_1) -> bb1 | bb2
    // bb1: _3 = move _2; goto bb3
    // bb2: goto bb3
    // bb3: _0 = move _2; return
    let mut body = Body::new("branch_merge_move_err", 2);
    body.locals = vec![
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(1))),
                targets: SwitchTargets::if_bool(BasicBlockId(1), BasicBlockId(2)),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                Place::from_local(LocalId(3)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            )],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    let errs = check_body(&body).expect_err("expected merge-point maybe-uninit error");
    assert!(
        errs.iter().any(|e| matches!(
            &e.kind,
            BorrowErrorKind::UseOfMaybeUninitialized { place } if *place == Place::from_local(LocalId(2))
        )),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn loop_second_iteration_read_after_move_is_rejected() {
    // bb0: goto bb1
    // bb1: _3 = move _2; switch(_1) -> bb1 | bb2
    // bb2: _0 = const 0; return
    let mut body = Body::new("loop_move_err", 2);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::String, Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                Place::from_local(LocalId(3)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            )],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(1))),
                targets: SwitchTargets::if_bool(BasicBlockId(1), BasicBlockId(2)),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(const_i64(0)),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    let errs = check_body(&body).expect_err("expected loop move error");
    assert!(
        errs.iter().any(|e| matches!(
            &e.kind,
            BorrowErrorKind::UseOfMaybeUninitialized { place } if *place == Place::from_local(LocalId(2))
        )),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn nll_allows_mutation_after_last_use_of_borrow() {
    // _1 = 10;
    // _2 = &mut _1;
    // *_2 = 20;     // last use of _2
    // _1 = 30;      // OK under NLL!
    // _0 = _1;
    let mut body = Body::new("nll_after_last_use_ok", 0);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(10))),
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(
                Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                Rvalue::Use(const_i64(20)),
            ),
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(30))),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(check_body(&body), Ok(()));
}

#[test]
fn nll_rejects_mutation_before_last_use_of_borrow() {
    // _1 = 10;
    // _2 = &mut _1;
    // _1 = 30;      // ERR: _2 is still live!
    // _0 = *_2;
    let mut body = Body::new("nll_before_last_use_err", 0);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(10))),
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(30))),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(
                    Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                )),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected borrow conflict");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, BorrowErrorKind::BorrowConflict { .. })),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn shared_loans_allow_multiple_shared_borrows_and_reads_but_block_mut_and_write() {
    // Valid: two shared borrows + read of _1 while both live
    let mut ok_body = Body::new("shared_ok", 0);
    ok_body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Not, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Not, LocalKind::Var),
    ];
    ok_body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(1))),
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(
                Place::from_local(LocalId(3)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(
                    Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                )),
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Copy(
                    Place::from_local(LocalId(3)).project(ProjectionElem::Deref),
                )),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    assert_eq!(check_body(&ok_body), Ok(()));

    // Invalid: shared borrow live while mutably borrowing _1
    let mut err_body = ok_body.clone();
    err_body.basic_blocks[0].statements[2] = assign(
        Place::from_local(LocalId(3)),
        Rvalue::Ref {
            borrow_kind: BorrowKind::Mut,
            place: Place::from_local(LocalId(1)),
        },
    );
    let errs = check_body(&err_body).expect_err("expected shared/mut borrow conflict");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, BorrowErrorKind::BorrowConflict { .. })),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn disjoint_field_loans_do_not_conflict() {
    // _1: Struct(0) (arg)
    // _2 = &mut _1.0
    // _3 = &mut _1.1
    // use *_2 and *_3
    let mut body = Body::new("disjoint_fields_ok", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: field_place(LocalId(1), 0),
                },
            ),
            assign(
                Place::from_local(LocalId(3)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: field_place(LocalId(1), 1),
                },
            ),
            assign(
                Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                Rvalue::Use(const_i64(1)),
            ),
            assign(
                Place::from_local(LocalId(3)).project(ProjectionElem::Deref),
                Rvalue::Use(const_i64(2)),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(check_body(&body), Ok(()));
}

#[test]
fn index_loan_conservatively_conflicts_with_other_indices() {
    // _1: ArrayOf (arg), _2: i64 (idx1), _3: i64 (idx2)
    // _4 = &mut _1[_2]
    // _5 = &_1[_3]     // conflicts!
    // use *_4 and *_5
    let mut body = Body::new("index_conflict_err", 3);
    body.locals = vec![
        LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::ArrayOf(0, 4), Mutability::Mut, LocalKind::Arg),
        LocalDecl::new(i64_ty(), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(i64_ty(), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Not, LocalKind::Var),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(4)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: index_place(LocalId(1), LocalId(2)),
                },
            ),
            assign(
                Place::from_local(LocalId(5)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: index_place(LocalId(1), LocalId(3)),
                },
            ),
            assign(
                Place::from_local(LocalId(4)).project(ProjectionElem::Deref),
                Rvalue::Use(const_i64(7)),
            ),
            assign(
                Place::from_local(LocalId(6)),
                Rvalue::Use(Operand::Copy(
                    Place::from_local(LocalId(5)).project(ProjectionElem::Deref),
                )),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected index loan conflict");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, BorrowErrorKind::BorrowConflict { .. })),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn escaping_local_reference_is_rejected() {
    // _1 = 42;
    // _2 = &_1;
    // _0 = move _2;
    // return;
    let mut body = Body::new("escaping_ref_err", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(42))),
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected escaping reference error");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, BorrowErrorKind::EscapingReference { .. })),
        "unexpected errors: {errs:?}"
    );
}

#[test]
fn returning_borrow_derived_from_input_borrow_param_is_allowed() {
    // fn first_field(_1: &Struct) -> &i64 {
    //   _2 = &(*_1).0;
    //   _0 = move _2;
    //   return;
    // }
    let mut body = Body::new("reborrow_input_ok", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::StructRef(0), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1))
                        .project(ProjectionElem::Deref)
                        .project(ProjectionElem::Field(0)),
                },
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(2)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_eq!(check_body(&body), Ok(()));
}

#[test]
fn reborrowing_local_storage_and_returning_is_rejected() {
    // _1 = 42;
    // _2 = &_1;
    // _3 = &(*_2);
    // _0 = move _3;
    // return;
    let mut body = Body::new("reborrow_local_escape_err", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Ptr(0), Mutability::Mut, LocalKind::Var),
    ];
    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(Place::from_local(LocalId(1)), Rvalue::Use(const_i64(42))),
            assign(
                Place::from_local(LocalId(2)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            assign(
                Place::from_local(LocalId(3)),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Shared,
                    place: Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                },
            ),
            assign(
                Place::from_local(LocalId(0)),
                Rvalue::Use(Operand::Move(Place::from_local(LocalId(3)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let errs = check_body(&body).expect_err("expected escaping reborrow of local");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, BorrowErrorKind::EscapingReference { .. })),
        "unexpected errors: {errs:?}"
    );
}
