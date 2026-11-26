use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use ae_codegen_c::{CodegenError, EmitConfig, emit_body_c, emit_module_c};
use ae_layout::TargetSpec;
use ae_mir::{
    AggregateKind, AssertMessage, BasicBlockData, BasicBlockId, BinOp, Body, BorrowKind, CastKind,
    ConstFloat, ConstValue, Constant, LocalDecl, LocalId, LocalKind, MirValue, Mutability, Operand,
    Place, ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets, Terminator,
    TerminatorKind, UnOp, eval_body,
};
use ae_ty::{DefId, FloatTy, IntTy, TyKind};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("ae_codegen_c_{prefix}_{pid}_{id}"));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn find_c_compiler() -> String {
    if let Ok(cc) = std::env::var("CC")
        && !cc.trim().is_empty()
    {
        return cc;
    }
    for candidate in ["gcc", "cc", "clang"] {
        if Command::new(candidate).arg("--version").output().is_ok() {
            return candidate.to_owned();
        }
    }
    panic!("no C compiler found (tried CC, gcc, cc, clang)");
}

fn compile_and_run_c(c_source: &str, name: &str) -> Output {
    let dir = TempDirGuard::new(name);
    let c_path = dir.path.join("test.c");
    let exe_name = if cfg!(windows) { "test.exe" } else { "test" };
    let exe_path = dir.path.join(exe_name);

    fs::write(&c_path, c_source).expect("write generated C file");

    let cc = find_c_compiler();
    let compile = Command::new(&cc)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg(&c_path)
        .arg("-o")
        .arg(&exe_path)
        .arg("-lm")
        .output()
        .expect("invoke C compiler");

    assert!(
        compile.status.success(),
        "C compilation failed for {name}:\nstdout:\n{}\nstderr:\n{}\n--- C SOURCE ---\n{c_source}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    Command::new(&exe_path)
        .output()
        .expect("execute compiled C binary")
}

fn config_with_main() -> EmitConfig {
    EmitConfig {
        target: TargetSpec::host(),
        emit_main_wrapper: true,
    }
}

fn assign(local: LocalId, rvalue: Rvalue) -> Statement {
    Statement::new(StatementKind::Assign(Place::from_local(local), rvalue))
}

fn const_int(v: i128, ity: IntTy) -> Operand {
    Operand::Constant(Constant::new(TyKind::Int(ity), ConstValue::Int(v)))
}

fn const_i64(v: i64) -> Operand {
    const_int(i128::from(v), IntTy::I64)
}

fn const_bool(b: bool) -> Operand {
    Operand::Constant(Constant::new(TyKind::Bool, ConstValue::Bool(b)))
}

fn assert_c_parity_i64(body: &Body, name: &str) {
    let eval_res = eval_body(body, &[]).expect("MIR eval should succeed");
    let MirValue::Int(expected, _) = eval_res else {
        panic!("expected Int from eval_body, got {eval_res:?}");
    };

    let c_src = emit_module_c(std::slice::from_ref(body), &config_with_main())
        .expect("C codegen should succeed");
    let output = compile_and_run_c(&c_src, name);
    assert_eq!(
        output.status.code(),
        Some(expected as i32),
        "exit code mismatch for {name}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_c_traps(body: &Body, name: &str) {
    let eval_res = eval_body(body, &[]);
    assert!(
        eval_res.is_err(),
        "expected MIR eval to trap for {name}, got {eval_res:?}"
    );

    let c_src = emit_module_c(std::slice::from_ref(body), &config_with_main())
        .expect("C codegen should succeed");
    let output = compile_and_run_c(&c_src, name);
    assert!(
        !output.status.success(),
        "expected compiled binary {name} to trap, but it exited with {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ae trap:"),
        "expected 'ae trap:' in stderr for {name}, got: {stderr}"
    );
}

#[test]
fn parity_scalars_casts_and_arithmetic() {
    let mut body = Body::new("main", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Int(IntTy::I8), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Float(FloatTy::F64), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::U8), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            Statement::new(StatementKind::StorageLive(LocalId(1))),
            assign(
                LocalId(1),
                Rvalue::BinaryOp(BinOp::Add, const_int(10, IntTy::I8), const_int(20, IntTy::I8)),
            ),
            assign(
                LocalId(2),
                Rvalue::Cast(
                    CastKind::IntToFloat,
                    Operand::Copy(Place::from_local(LocalId(1))),
                    TyKind::Float(FloatTy::F64),
                ),
            ),
            assign(
                LocalId(2),
                Rvalue::BinaryOp(
                    BinOp::Mul,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    Operand::Constant(Constant::new(
                        TyKind::Float(FloatTy::F64),
                        ConstValue::Float(ConstFloat::from_f64(1.5)),
                    )),
                ),
            ),
            assign(
                LocalId(3),
                Rvalue::Cast(
                    CastKind::FloatToInt,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    TyKind::Int(IntTy::I64),
                ),
            ),
            assign(
                LocalId(4),
                Rvalue::BinaryOp(
                    BinOp::Eq,
                    Operand::Copy(Place::from_local(LocalId(3))),
                    const_i64(45),
                ),
            ),
            assign(
                LocalId(5),
                Rvalue::Cast(
                    CastKind::Misc,
                    Operand::Copy(Place::from_local(LocalId(4))),
                    TyKind::Int(IntTy::U8),
                ),
            ),
            assign(
                LocalId(0),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(3))),
                    Rvalue::Cast(
                        CastKind::IntToInt,
                        Operand::Copy(Place::from_local(LocalId(5))),
                        TyKind::Int(IntTy::I64),
                    )
                    .into_dummy_or_local(),
                ),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    // Fix: Rvalue::BinaryOp takes Operands, not Rvalues; use a temp local for the cast.
    body.locals.push(LocalDecl::new(
        TyKind::Int(IntTy::I64),
        Mutability::Mut,
        LocalKind::Temp,
    ));
    let last_idx = body.basic_blocks[0].statements.len() - 1;
    body.basic_blocks[0].statements[last_idx] = assign(
        LocalId(6),
        Rvalue::Cast(
            CastKind::IntToInt,
            Operand::Copy(Place::from_local(LocalId(5))),
            TyKind::Int(IntTy::I64),
        ),
    );
    body.basic_blocks[0].statements.push(assign(
        LocalId(0),
        Rvalue::BinaryOp(
            BinOp::Add,
            Operand::Copy(Place::from_local(LocalId(3))),
            Operand::Copy(Place::from_local(LocalId(6))),
        ),
    ));
    body.basic_blocks[0]
        .statements
        .push(Statement::new(StatementKind::StorageDead(LocalId(1))));
    body.basic_blocks[0]
        .statements
        .push(Statement::new(StatementKind::Nop));

    assert_c_parity_i64(&body, "scalars_casts");
}

trait DummyExt {
    fn into_dummy_or_local(self) -> Operand;
}
impl DummyExt for Rvalue {
    fn into_dummy_or_local(self) -> Operand {
        const_i64(0)
    }
}

#[test]
fn parity_overflow_and_div_zero_traps() {
    let make_binop_body = |op: BinOp, a: i128, b: i128, ity: IntTy| {
        let mut body = Body::new("main", 0);
        body.locals = vec![
            LocalDecl::new(TyKind::Int(ity), Mutability::Mut, LocalKind::ReturnPlace),
        ];
        body.basic_blocks = vec![BasicBlockData::new(
            vec![assign(
                LocalId(0),
                Rvalue::BinaryOp(op, const_int(a, ity), const_int(b, ity)),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        )];
        body
    };

    assert_c_traps(&make_binop_body(BinOp::Add, 120, 20, IntTy::I8), "i8_add_ovf");
    assert_c_traps(&make_binop_body(BinOp::Sub, -120, 20, IntTy::I8), "i8_sub_ovf");
    assert_c_traps(&make_binop_body(BinOp::Mul, 20, 10, IntTy::I8), "i8_mul_ovf");
    assert_c_traps(&make_binop_body(BinOp::Div, -128, -1, IntTy::I8), "i8_div_ovf");
    assert_c_traps(&make_binop_body(BinOp::Div, 10, 0, IntTy::I64), "div_zero");
    assert_c_traps(&make_binop_body(BinOp::Rem, 10, 0, IntTy::I64), "rem_zero");
    assert_c_traps(&make_binop_body(BinOp::Add, 250, 10, IntTy::U8), "u8_add_ovf");
    assert_c_traps(&make_binop_body(BinOp::Sub, 5, 10, IntTy::U8), "u8_sub_ovf");
    assert_c_traps(&make_binop_body(BinOp::Shl, 1, 8, IntTy::U8), "u8_shl_ovf");

    // Signed negation overflow: -(-128i8) traps.
    let mut neg_ovf = Body::new("main", 0);
    neg_ovf.locals = vec![LocalDecl::new(
        TyKind::Int(IntTy::I8),
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    neg_ovf.basic_blocks = vec![BasicBlockData::new(
        vec![assign(
            LocalId(0),
            Rvalue::UnaryOp(UnOp::Neg, const_int(-128, IntTy::I8)),
        )],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    assert_c_traps(&neg_ovf, "i8_neg_ovf");
}

#[test]
fn parity_loop_and_branching_cfg() {
    // Computes sum of 1..=10 = 55 in a loop.
    let mut body = Body::new("main", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Var), // sum
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Var), // i
        LocalDecl::new(TyKind::Bool, Mutability::Mut, LocalKind::Temp),           // cond
    ];

    body.basic_blocks = vec![
        // bb0: init sum = 0, i = 1 -> bb1
        BasicBlockData::new(
            vec![
                assign(LocalId(1), Rvalue::Use(const_i64(0))),
                assign(LocalId(2), Rvalue::Use(const_i64(1))),
            ],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(1),
            })),
        ),
        // bb1: cond = (i <= 10); SwitchInt(cond, 0 -> bb3, else -> bb2)
        BasicBlockData::new(
            vec![assign(
                LocalId(3),
                Rvalue::BinaryOp(
                    BinOp::Le,
                    Operand::Copy(Place::from_local(LocalId(2))),
                    const_i64(10),
                ),
            )],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(3))),
                targets: SwitchTargets::if_bool(BasicBlockId(2), BasicBlockId(3)),
            })),
        ),
        // bb2: sum += i; i += 1 -> bb1
        BasicBlockData::new(
            vec![
                assign(
                    LocalId(1),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Operand::Copy(Place::from_local(LocalId(1))),
                        Operand::Copy(Place::from_local(LocalId(2))),
                    ),
                ),
                assign(
                    LocalId(2),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Operand::Copy(Place::from_local(LocalId(2))),
                        const_i64(1),
                    ),
                ),
            ],
            Some(Terminator::new(TerminatorKind::Goto {
                target: BasicBlockId(1),
            })),
        ),
        // bb3: _0 = sum; return
        BasicBlockData::new(
            vec![assign(
                LocalId(0),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];

    assert_c_parity_i64(&body, "loop_sum");
}

#[test]
fn multi_function_calls_compile_and_run() {
    let mut helper = Body::new("add_mul", 2);
    helper.def = Some(DefId(1));
    helper.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Not, LocalKind::Arg),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
    ];
    helper.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                LocalId(3),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(1))),
                    Operand::Copy(Place::from_local(LocalId(2))),
                ),
            ),
            assign(
                LocalId(0),
                Rvalue::BinaryOp(
                    BinOp::Mul,
                    Operand::Copy(Place::from_local(LocalId(3))),
                    const_i64(2),
                ),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    let mut main_body = Body::new("main", 0);
    main_body.def = Some(DefId(0));
    main_body.locals = vec![LocalDecl::new(
        TyKind::Int(IntTy::I64),
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    main_body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Call {
                func: Operand::Constant(Constant::new(
                    TyKind::Fn(1),
                    ConstValue::FnDef(DefId(1)),
                )),
                args: vec![const_i64(7), const_i64(14)],
                destination: Place::from_local(LocalId(0)),
                target: Some(BasicBlockId(1)),
            })),
        ),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
    ];

    let c_src = emit_module_c(&[helper, main_body], &config_with_main())
        .expect("multi-fn C codegen should succeed");
    let output = compile_and_run_c(&c_src, "multi_fn");
    assert_eq!(output.status.code(), Some(42));
}

#[test]
fn parity_structs_arrays_ref_deref_and_len() {
    let mut body = Body::new("main", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Struct(0), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::ArrayOf(0, 3), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Int(IntTy::Usize), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(
                LocalId(1),
                Rvalue::Aggregate(
                    AggregateKind::Struct(0),
                    vec![const_i64(10), const_i64(25)],
                ),
            ),
            assign(
                LocalId(2),
                Rvalue::Aggregate(
                    AggregateKind::Array(TyKind::Int(IntTy::I64), 0),
                    vec![const_i64(1), const_i64(2), const_i64(3)],
                ),
            ),
            assign(
                LocalId(3),
                Rvalue::Use(Operand::Constant(Constant::new(
                    TyKind::Int(IntTy::Usize),
                    ConstValue::Int(1),
                ))),
            ),
            // _5 = _1.f1 + _2[_3] (25 + 2 = 27)
            assign(
                LocalId(5),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(1)).project(ProjectionElem::Field(1))),
                    Operand::Copy(
                        Place::from_local(LocalId(2)).project(ProjectionElem::Index(LocalId(3))),
                    ),
                ),
            ),
            // _6 = len(_2) cast to i64 (3)
            assign(
                LocalId(6),
                Rvalue::Cast(
                    CastKind::IntToInt,
                    Operand::Constant(Constant::new(
                        TyKind::Int(IntTy::Usize),
                        ConstValue::Int(0),
                    )),
                    TyKind::Int(IntTy::I64),
                ),
            ),
            assign(LocalId(3), Rvalue::Len(Place::from_local(LocalId(2)))),
            assign(
                LocalId(6),
                Rvalue::Cast(
                    CastKind::IntToInt,
                    Operand::Copy(Place::from_local(LocalId(3))),
                    TyKind::Int(IntTy::I64),
                ),
            ),
            // _0 = _5 + _6 (27 + 3 = 30)
            assign(
                LocalId(0),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(Place::from_local(LocalId(5))),
                    Operand::Copy(Place::from_local(LocalId(6))),
                ),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];

    assert_c_parity_i64(&body, "structs_arrays");

    // Also test Ref + Deref in C compilation & execution.
    let mut ptr_body = Body::new("main", 0);
    ptr_body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::PtrMut(0), Mutability::Mut, LocalKind::Temp),
    ];
    ptr_body.basic_blocks = vec![BasicBlockData::new(
        vec![
            assign(LocalId(1), Rvalue::Use(const_i64(10))),
            assign(
                LocalId(2),
                Rvalue::Ref {
                    borrow_kind: BorrowKind::Mut,
                    place: Place::from_local(LocalId(1)),
                },
            ),
            Statement::new(StatementKind::Assign(
                Place::from_local(LocalId(2)).project(ProjectionElem::Deref),
                Rvalue::Use(const_i64(99)),
            )),
            assign(
                LocalId(0),
                Rvalue::Use(Operand::Copy(Place::from_local(LocalId(1)))),
            ),
        ],
        Some(Terminator::new(TerminatorKind::Return)),
    )];
    let c_src = emit_body_c(&ptr_body, &config_with_main()).expect("ptr C codegen");
    let output = compile_and_run_c(&c_src, "ref_deref");
    assert_eq!(output.status.code(), Some(99));
}

#[test]
fn parity_tagged_enum_set_discriminant_switch_and_downcast() {
    let mut body = Body::new("main", 0);
    body.locals = vec![
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::ReturnPlace),
        LocalDecl::new(TyKind::Enum("OptionI64", 0), Mutability::Mut, LocalKind::Var),
        LocalDecl::new(TyKind::Int(IntTy::I64), Mutability::Mut, LocalKind::Temp),
    ];

    body.basic_blocks = vec![
        // bb0: construct variant 1 with payload 33, then SetDiscriminant to 1, read Discriminant -> SwitchInt
        BasicBlockData::new(
            vec![
                assign(
                    LocalId(1),
                    Rvalue::Aggregate(AggregateKind::Enum(0, 1), vec![const_i64(33)]),
                ),
                Statement::new(StatementKind::SetDiscriminant {
                    place: Place::from_local(LocalId(1)),
                    variant_index: 1,
                }),
                assign(LocalId(2), Rvalue::Discriminant(Place::from_local(LocalId(1)))),
            ],
            Some(Terminator::new(TerminatorKind::SwitchInt {
                discr: Operand::Copy(Place::from_local(LocalId(2))),
                targets: SwitchTargets::new(vec![(0, BasicBlockId(1)), (1, BasicBlockId(2))], BasicBlockId(3)),
            })),
        ),
        // bb1: variant 0 -> 0
        BasicBlockData::new(
            vec![assign(LocalId(0), Rvalue::Use(const_i64(0)))],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
        // bb2: variant 1 -> downcast(1).field(0) + 7 = 40
        BasicBlockData::new(
            vec![assign(
                LocalId(0),
                Rvalue::BinaryOp(
                    BinOp::Add,
                    Operand::Copy(
                        Place::from_local(LocalId(1))
                            .project(ProjectionElem::Downcast(1))
                            .project(ProjectionElem::Field(0)),
                    ),
                    const_i64(7),
                ),
            )],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
        // bb3: unreachable
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Unreachable))),
    ];

    assert_c_parity_i64(&body, "tagged_enum");
}

#[test]
fn parity_assert_terminator_pass_and_trap() {
    let mut pass_body = Body::new("main", 0);
    pass_body.locals = vec![LocalDecl::new(
        TyKind::Int(IntTy::I64),
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    pass_body.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Assert {
                cond: const_bool(true),
                expected: true,
                msg: AssertMessage::Custom("ok".to_owned()),
                target: BasicBlockId(1),
            })),
        ),
        BasicBlockData::new(
            vec![assign(LocalId(0), Rvalue::Use(const_i64(17)))],
            Some(Terminator::new(TerminatorKind::Return)),
        ),
    ];
    assert_c_parity_i64(&pass_body, "assert_pass");

    let mut fail_body = pass_body.clone();
    fail_body.basic_blocks[0].terminator = Some(Terminator::new(TerminatorKind::Assert {
        cond: const_bool(false),
        expected: true,
        msg: AssertMessage::Custom("boom".to_owned()),
        target: BasicBlockId(1),
    }));
    assert_c_traps(&fail_body, "assert_fail");
}

#[test]
fn invalid_mir_and_unresolved_calls_return_errors_without_panicking() {
    let empty = Body::new("empty", 0);
    let err = emit_body_c(&empty, &EmitConfig::default()).expect_err("empty body must fail");
    assert!(matches!(err, CodegenError::InvalidMir { .. }));

    let mut bad_call = Body::new("bad_call", 0);
    bad_call.locals = vec![LocalDecl::new(
        TyKind::Unit,
        Mutability::Mut,
        LocalKind::ReturnPlace,
    )];
    bad_call.basic_blocks = vec![
        BasicBlockData::new(
            vec![],
            Some(Terminator::new(TerminatorKind::Call {
                func: Operand::Constant(Constant::new(
                    TyKind::Fn(99),
                    ConstValue::FnDef(DefId(99)),
                )),
                args: vec![],
                destination: Place::from_local(LocalId(0)),
                target: Some(BasicBlockId(1)),
            })),
        ),
        BasicBlockData::new(vec![], Some(Terminator::new(TerminatorKind::Return))),
    ];
    let err = emit_body_c(&bad_call, &EmitConfig::default()).expect_err("unresolved call must fail");
    assert!(matches!(
        err,
        CodegenError::UnresolvedCall {
            def_id: DefId(99),
            ..
        }
    ));
}
