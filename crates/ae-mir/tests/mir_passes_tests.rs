use std::collections::BTreeSet;

use ae_mir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, CastKind, Cfg,
    ConstFloat, ConstValue, Constant, InitAnalysis, InitState, LivenessAnalysis, LocalDecl,
    LocalId, LocalKind, MirEvalError, MirValue, Mutability, Operand, Place, ProjectionElem, Rvalue,
    Statement, StatementKind, SwitchTargets, Terminator, TerminatorKind, UnOp, eval_body,
    eval_body_with_limit, optimize_body, validate,
};
use ae_ty::{FloatTy, IntTy, TyKind};

fn i64_ty() -> TyKind {
    TyKind::Int(IntTy::I64)
}

fn i8_ty() -> TyKind {
    TyKind::Int(IntTy::I8)
}

fn const_i64(v: i128) -> Operand {
    Operand::Constant(Constant::new(i64_ty(), ConstValue::Int(v)))
}

fn const_i8(v: i128) -> Operand {
    Operand::Constant(Constant::new(i8_ty(), ConstValue::Int(v)))
}

fn const_bool(b: bool) -> Operand {
    Operand::Constant(Constant::new(TyKind::Bool, ConstValue::Bool(b)))
}

fn assign(local: LocalId, rvalue: Rvalue) -> Statement {
    Statement::new(StatementKind::Assign(Place::from_local(local), rvalue))
}

#[test]
fn cfg_predecessors_successors_and_rpo() {
    // Diamond CFG: bb0 -> (bb1, bb2) -> bb3, plus unreachable bb4
    let mut body = Body::new("diamond", 1);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg),
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
            vec![assign(LocalId(0), Rvalue::Use(const_i64(10)))],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(
            vec![assign(LocalId(0), Rvalue::Use(const_i64(20)))],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Unreachable))),
    ];

    let cfg = Cfg::compute(&body);
    assert_eq!(
        cfg.successors(BasicBlockId(0)),
        &[BasicBlockId(2), BasicBlockId(1)]
    );
    assert_eq!(
        cfg.predecessors(BasicBlockId(3)),
        &[BasicBlockId(1), BasicBlockId(2)]
    );
    assert!(cfg.is_reachable(BasicBlockId(3)));
    assert!(!cfg.is_reachable(BasicBlockId(4)));

    // RPO must visit bb0 first and bb3 last among reachable blocks.
    let rpo = cfg.rpo();
    assert_eq!(rpo.len(), 4);
    assert_eq!(rpo[0], BasicBlockId(0));
    assert_eq!(rpo[3], BasicBlockId(3));
}

#[test]
fn init_dataflow_tracks_uninit_init_maybe_and_moves() {
    // _0: i64, _1: bool (arg), _2: i64 (init only in true branch), _3: i64 (init in both, moved in false)
    let mut body = Body::new("init_flow", 1);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Bool, Mutability::Not, LocalKind::Arg),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![
                Statement::new(StatementKind::StorageLive(LocalId(2))),
                Statement::new(StatementKind::StorageLive(LocalId(3))),
                assign(LocalId(3), Rvalue::Use(const_i64(99))),
            ],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(1))),
                targets: SwitchTargets::if_bool(BasicBlockId(1), BasicBlockId(2)),
            })),
        ),
        BasicBlockData::new(
            vec![
                assign(LocalId(2), Rvalue::Use(const_i64(1))),
                assign(LocalId(0), Rvalue::Use(Operand::Copy(Place::from_local(LocalId(3))))),
            ],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(
            vec![
                assign(LocalId(4), Rvalue::Use(Operand::Move(Place::from_local(LocalId(3))))),
                assign(LocalId(0), Rvalue::Use(Operand::Copy(Place::from_local(LocalId(4))))),
            ],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(3),
            })),
        ),
        BasicBlockData::new(
            vec![Statement::new(StatementKind::StorageDead(LocalId(4)))],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    validate(&body).expect("valid body");
    let init = InitAnalysis::analyze(&body);

    // On entry to bb0: _1 (arg) is Init, _0/_2/_3/_4 are Uninit.
    assert!(init.is_definitely_init(BasicBlockId(0), LocalId(1)));
    assert!(!init.is_maybe_init(BasicBlockId(0), LocalId(2)));

    // On entry to bb3 (merge):
    // _0 is Init (assigned on both branches)
    // _2 is MaybeInit (assigned only on bb1)
    // _3 is MaybeInit (Init on bb1, moved/Uninit on bb2)
    assert_eq!(init.entry(BasicBlockId(3))[0], InitState::Init);
    assert_eq!(init.entry(BasicBlockId(3))[2], InitState::MaybeInit);
    assert_eq!(init.entry(BasicBlockId(3))[3], InitState::MaybeInit);
    // _4 had StorageDead in bb3, so at exit of bb3 it is Uninit.
    assert_eq!(init.exit(BasicBlockId(3))[4], InitState::Uninit);
}

#[test]
fn liveness_analysis_across_loop_and_return() {
    // bb0: _2 = _1; goto bb1
    // bb1: _3 = _2 > 0; switch(_3) { 0 -> bb3, _ -> bb2 }
    // bb2: _2 = _2 - 1; goto bb1
    // bb3: _0 = _2; return
    let mut body = Body::new("loop_live", 1);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![assign(
                LocalId(2),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
            )],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                LocalId(3),
                Rvalue::BinaryOp(
                    BinOp::Gt,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    const_i64(0),
                ),
            )],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(3))),
                targets: SwitchTargets::if_bool(BasicBlockId(2), BasicBlockId(3)),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                LocalId(2),
                Rvalue::BinaryOp(
                    BinOp::Sub,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    const_i64(1),
                ),
            )],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(
            vec![assign(
                LocalId(0),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(2)))),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    let live = LivenessAnalysis::analyze(&body);
    assert_eq!(live.live_in(BasicBlockId(0)), &BTreeSet::from([LocalId(1)]));
    assert_eq!(live.live_in(BasicBlockId(1)), &BTreeSet::from([LocalId(2)]));
    assert_eq!(live.live_in(BasicBlockId(2)), &BTreeSet::from([LocalId(2)]));
    assert_eq!(live.live_in(BasicBlockId(3)), &BTreeSet::from([LocalId(2)]));
}

#[test]
fn optimize_folds_constants_threads_jumps_and_prunes_blocks_preserving_eval() {
    // bb0: _1 = 2 + 3; _2 = _1 * 4; _3 = _2 == 20; switch(_3) { 0 -> bb4, _ -> bb1 }
    // bb1: empty Goto(bb2)
    // bb2: empty Goto(bb3)
    // bb3: _0 = _2 + 1; Return
    // bb4: _0 = 999; Return (dead branch)
    let mut body = Body::new("fold_and_prune", 0);
    body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![
        BasicBlockData::new(
            vec![
                assign(LocalId(1), Rvalue::BinaryOp(BinOp::Add, const_i64(2), const_i64(3))),
                assign(
                    LocalId(2),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Operand::Copy(Place::from_local(LocalId(1))),
                        const_i64(4),
                    ),
                ),
                assign(
                    LocalId(3),
                    Rvalue::BinaryOp(
                        BinOp::Eq,
                        Operand::Copy(Place::from_local(LocalId(2))),
                        const_i64(20),
                    ),
                ),
            ],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(3))),
                targets: SwitchTargets::if_bool(BasicBlockId(1), BasicBlockId(4)),
            })),
        ),
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(2),
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
                LocalId(0),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    const_i64(1),
                ),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
        BasicBlockData::new(
            vec![assign(LocalId(0), Rvalue::Use(const_i64(999)))],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    validate(&body).expect("valid before opt");
    let before_val = eval_body(&body, &[]).expect("eval before opt");
    assert_eq!(before_val, MirValue::i64(21));

    let before_blocks = body.basic_blocks.len();
    let stats = optimize_body(&mut body);
    validate(&body).expect("valid after opt");

    assert!(stats.constants_folded >= 4);
    assert!(stats.jumps_threaded >= 1);
    assert_eq!(stats.blocks_pruned, 3); // bb1, bb2, bb4 pruned
    assert!(body.basic_blocks.len() < before_blocks);

    let after_val = eval_body(&body, &[]).expect("eval after opt");
    assert_eq!(before_val, after_val);
}

#[test]
fn optimize_preserves_trapping_constants() {
    // Div by zero must NOT be folded away.
    let mut div_zero = Body::new("div_zero", 0);
    div_zero.locals = vec![LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace)];
    div_zero.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            LocalId(0),
            Rvalue::BinaryOp(BinOp::Div, const_i64(10), const_i64(0)),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let before = eval_body(&div_zero, &[]);
    let stats = optimize_body(&mut div_zero);
    validate(&div_zero).expect("valid after opt");
    let after = eval_body(&div_zero, &[]);
    assert_eq!(stats.constants_folded, 0);
    assert_eq!(before, Err(MirEvalError::DivisionByZero { block: BasicBlockId(0) }));
    assert_eq!(before, after);

    // Signed i8 overflow must NOT be folded away.
    let mut overflow = Body::new("overflow", 0);
    overflow.locals = vec![LocalDecl::new(i8_ty(), Mutability::Mut, LocalKind::ReturnPlace)];
    overflow.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            LocalId(0),
            Rvalue::BinaryOp(BinOp::Add, const_i8(120), const_i8(20)),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let before_ov = eval_body(&overflow, &[]);
    let stats_ov = optimize_body(&mut overflow);
    validate(&overflow).expect("valid after opt");
    let after_ov = eval_body(&overflow, &[]);
    assert_eq!(stats_ov.constants_folded, 0);
    assert!(matches!(before_ov, Err(MirEvalError::IntegerOverflow { .. })));
    assert_eq!(before_ov, after_ov);
}

#[test]
fn eval_traps_uninit_use_after_move_assert_unreachable_and_step_limit() {
    // 1. Uninitialized read
    let mut uninit_body = Body::new("uninit", 0);
    uninit_body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
    ];
    uninit_body.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            LocalId(0),
            Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    assert_eq!(
        eval_body(&uninit_body, &[]),
        Err(MirEvalError::UninitializedRead {
            block: BasicBlockId(0),
            local: LocalId(1),
        })
    );

    // 2. Use after move
    let mut uam_body = Body::new("uam", 0);
    uam_body.locals = vec![
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
    ];
    uam_body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(LocalId(1), Rvalue::Use(const_i64(7))),
            assign(LocalId(2), Rvalue::Use(Operand::Move(Place::from_local(LocalId(1))))),
            assign(LocalId(0), Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1))))),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    assert_eq!(
        eval_body(&uam_body, &[]),
        Err(MirEvalError::UseAfterMove {
            block: BasicBlockId(0),
            local: LocalId(1),
        })
    );

    // 3. Assert failure
    let mut assert_body = Body::new("assert_fail", 0);
    assert_body.locals = vec![LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace)];
    assert_body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Assert {
                cond: const_bool(false),
                expected: true,
                msg: AssertMessage::Custom("boom".to_string()),
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
    ];
    assert_eq!(
        eval_body(&assert_body, &[]),
        Err(MirEvalError::AssertionFailed {
            block: BasicBlockId(0),
            message: "boom".to_string(),
        })
    );

    // 4. Unreachable executed
    let mut unreach_body = Body::new("unreach", 0);
    unreach_body.locals = vec![LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace)];
    unreach_body.basic_blocks = vec![BasicBlockData::new(
        vec![],
        Some(Terminator::new(TerminatorKind::Unreachable)),
    )];
    assert_eq!(
        eval_body(&unreach_body, &[]),
        Err(MirEvalError::UnreachableExecuted {
            block: BasicBlockId(0),
        })
    );

    // 5. Infinite loop step guard
    let mut inf_body = Body::new("inf", 0);
    inf_body.locals = vec![LocalDecl::new(TyKind::Unit, Mutability::Mut, LocalKind::ReturnPlace)];
    inf_body.basic_blocks = vec![BasicBlockData::new(
        vec![],
        Some(Terminator::new(TerminatorKind::Goto {
            target: BasicBlockId(0),
        })),
    )];
    assert_eq!(
        eval_body_with_limit(&inf_body, &[], 50),
        Err(MirEvalError::StepLimitExceeded { limit: 50 })
    );
}

#[test]
fn eval_aggregates_projections_casts_and_floats() {
    let mut body = Body::new("agg_cast", 1);
    body.locals = vec![
        LocalDecl::new(TyKind::Float(FloatTy::F64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(i64_ty(), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(i64_ty(), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                LocalId(2),
                Rvalue::Aggregate(
                    AggregateKind::Struct(0),
                    vec![Operand::Copy(Place::from_local(LocalId(1))), const_i64(8)],
                ),
            ),
            assign(
                LocalId(3),
                Rvalue::Use(Operand::Copy(
                    Place::from_local(LocalId(2)).project(ProjectionElem::Field(0)),
                )),
            ),
            assign(
                LocalId(4),
                Rvalue::UnaryOp(UnOp::Neg, Operand::Copy(Place::from_local(LocalId(3)))),
            ),
            assign(
                LocalId(0),
                Rvalue::Cast(
                    CastKind::IntToFloat,
                    Operand::Copy(Place::from_local(LocalId(4))),
                    TyKind::Float(FloatTy::F64),
                ),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let res = eval_body(&body, &[MirValue::i64(12)]).expect("eval succeeds");
    assert_eq!(res, MirValue::Float(-12.0, FloatTy::F64));

    // Also verify ConstFloat roundtrip in UnaryOp::Neg
    let mut float_body = Body::new("float_neg", 0);
    float_body.locals = vec![LocalDecl::new(
        TyKind::Float(FloatTy::F64),
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    float_body.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            LocalId(0),
            Rvalue::UnaryOp(
                UnOp::Neg,
                Operand::Constant(Constant::new(
                    TyKind::Float(FloatTy::F64),
                    ConstValue::Float(ConstFloat::from_f64(3.5)),
                )),
            ),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    assert_eq!(eval_body(&float_body, &[]), Ok(MirValue::f64(-3.5)));
}
