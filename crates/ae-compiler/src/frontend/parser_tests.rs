use super::*;

    #[test]
    pub(super) fn scalar_output_matches_resolver_definition_to_live_slot() {
        let source = "struct Point { x: i64, y: i64 } struct Inner { x: i64 } struct Outer { inner: Inner, tag: i64 } struct Holder { ready: bool, text: string } struct Guard { id: i64 } fn scalar() -> bool { let local = (2 + 3) * 4; return local == 20; } fn mismatch() -> i64 { return true; } fn branches() -> i64 { if false { let same = 1; return same; } else if true { let same = 2; return same; } else { let same = 3; return same; } } fn loops() -> i64 { while true { while false { continue; } break; } return 7; } fn bad_loop() -> i64 { continue; return 0; } fn ranges() -> i64 { for i in 2..5 { while false { continue; } if i == 3 { continue; } } let i = 7; return i; } fn bad_range() -> i64 { for j in false..5 {} return 0; } fn countdown() -> i64 { var n = 4; n = 2; while n > 0 { n -= 1; } return n; } fn frozen() -> i64 { let n = 1; n = 2; return n; } fn output() -> i64 { print(2); assert(2 == 2); return 0; } fn bad_assert() -> i64 { assert(5); return 0; } fn entry_shape() -> i64 { print(23); } fn calls() -> i64 { return compute(compute(2)); } fn wrong_call() -> i64 { return compute(false); } fn cross_module() -> i64 { return imported_scalar(2); } fn private_module() -> i64 { return imported_private(3); } fn local_array() -> i64 { let a = [4, 5]; return a[1]; } fn bad_array() -> i64 { let a = [4, 5]; return a[false]; } fn local_array_store() -> i64 { var a = [4, 5]; a[1] = 9; return a[1]; } fn bad_store() -> i64 { var a = [4, 5]; a[false] = 1; return a[0]; } fn frozen_store() -> i64 { let a = [4, 5]; a[0] = 9; return a[0]; } fn bad_store_value() -> i64 { var a = [4, 5]; a[0] = false; return a[0]; } fn compound_store() -> i64 { var a = [4, 5]; a[0] += 1; return a[0]; } fn array_param(a: &[i64; 2]) -> i64 { return a[1]; } fn array_param_store(a: &[i64; 2]) -> i64 { a[0] = 3; return 0; } fn field_read(p: &Point) -> i64 { return p.x + p.y; } fn field_text(h: &Holder) -> i64 { return h.text; } fn nested_read(o: &Outer) -> i64 { return o.inner.x; } fn make_point() -> i64 { let p = Point { x: 1, y: 2 }; return p.x + p.y; } fn make_guard() -> i64 { let g = Guard { id: 1 }; return g.id; } fn consume_point(p: Point) -> i64 { return p.x; } fn pass_point() -> i64 { let q = Point { x: 3, y: 4 }; let r = consume_point(q); return r + 1; } fn pass_twice() -> i64 { let q = Point { x: 5, y: 6 }; let r = consume_point(q); let s = consume_point(q); return r + s; } fn loop_move(b: bool) -> i64 { let q = Point { x: 7, y: 8 }; while b { let r = consume_point(q); } return 0; } fn unsafe_wrap() -> i64 { unsafe { let a = 1; } return 2; } fn compute(x: i64) -> i64 { return x; } fn consume(s: string) -> i64 { return 1; } fn main() { let text = \"hello\"; let flag = true; var value = flag; if flag {} print(flag); print(compute(2)); assert(consume(text)); print(consume(text)); let v = vec[1.0, 2.0]; print(vec[1]); print(v[0]); let raw = &value as *const bool; let view = raw as *mut f64; let write = &mut value as *mut bool; print(compute(choose[i64, vec[f64]](2, vec[1.0]))); let pair = Pair[i64, vec[f64]] { first: 1, second: vec[1.0] }; let made = Pair[i64, vec[f64]]::new(); let variant = Choice[i64, vec[f64]]::Some(1); let empty = Choice[i64, vec[f64]]::empty(); let compound = raw as *const shared[i64]; task_spawn(ex, || -> i64 { return 0; }); let closure = |x: i64| -> bool { return x == 0; }; }";
        let source = format!("{source} fn generic_scalar[T](generic_value: T) -> T {{ return generic_value; }}");
        let source = format!("{source} fn closure_owner(captured_scalar: i64) -> i64 {{ let f = || -> i64 {{ return captured_scalar; }}; return f(); }} fn closure_text() -> i64 {{ let keep = \"world\"; let g = || -> i64 {{ print(keep); return 0; }}; return g(); }} fn closure_vector() -> i64 {{ let numbers = vec[10, 20]; let get = || -> i64 {{ print(numbers[1]); return 0; }}; return get(); }} fn closure_words() -> i64 {{ let labels = vec[\"aa\", \"bb\"]; let pick = || -> i64 {{ print(labels[0]); return 0; }}; return pick(); }}");
        let source = format!("{source} struct ScalarBox[T] {{ item: T }} impl ScalarBox[T] {{ fn scalar_identity(method_value: T) -> T {{ return method_value; }} fn get(&self) -> T {{ return self.item; }} fn set(&mut self, new_value: T) -> T {{ self.item = new_value; return self.item; }} fn consume_box(self) -> T {{ return self.item; }} }}");
        let source = format!("{source} impl ScalarBox[T] {{ fn round_trip(self, route: bool) -> Self {{ if route {{ return self; }} else {{ return (self); }} }} }}");
        let source = format!("{source} fn take_box(taken: ScalarBox[i64]) -> i64 {{ return taken.item; }} impl ScalarBox[T] {{ fn send(self) -> i64 {{ return take_box((self)); }} fn send_twice(self) -> i64 {{ let sent = take_box(self); return self.item; }} }}");
        let source = format!("{source} fn bad_point() -> i64 {{ let wrong = Point {{ x: false, y: 1 }}; return wrong.y; }}");
        let source = format!("{source} fn bad_fields() -> i64 {{ let extra = Point {{ z: 1 }}; return 0; }} fn dup_fields() -> i64 {{ let dup = Point {{ x: 1, x: false }}; return 0; }} fn missing_fields() -> i64 {{ let missing = Point {{ x: 1 }}; return 0; }}");
        let source = source.as_str();
        let source = format!("{source} enum CachedBox[T] {{ Item(T) }} fn pack(specialized_text: string) -> CachedBox[string] {{ return CachedBox[string]::Item(specialized_text); }}");
        let source = format!("{source} struct CachedRecord[T] {{ value: T, count: i64 }} fn record(record_text: string) -> CachedRecord[string] {{ return CachedRecord[string] {{ count: 2, value: record_text }}; }}");
        let source = format!("{source} fn relay(call_text: string) -> string {{ return generic_scalar[string](call_text); }}");
        let source = format!("{source} fn use_import() -> i64 {{ return imported_generic[i64](7); }}");
        let source = format!("{source} fn write_ptr(raw_param: *mut i64) -> i64 {{ unsafe {{ *raw_param = 7; }} return 0; }}");
        let source = format!("{source} fn read_ptr(read_param: *const i64) -> i64 {{ unsafe {{ return read_param.deref(); }} }}");
        let source = format!("{source} fn local_pointer() -> i64 {{ var pointee = 1; let local_ptr = &mut pointee as *mut i64; unsafe {{ *local_ptr = 7; return local_ptr.deref(); }} }}");
        let source = format!("{source} fn vector_sum(vector_arg: &vec[f64]) -> f64 {{ var total = 0.0; for item in vector_arg {{ total += item; }} return total; }}");
        let source = format!("{source} fn owned_loop() -> i64 {{ let empty_vec = vec[]; for unused in empty_vec {{ print(unused); }} return 0; }}");
        let source = format!("{source} fn moved_vec() -> i64 {{ spawn(|| -> i64 {{ return 0; }}); return 0; }}");
        let source = format!("{source} fn print_words(words: &vec[string]) -> i64 {{ for word in words {{ print(word); }} return 0; }}");
        let source = format!("{source} fn scan_words() -> i64 {{ let text_base = \"a b\"; let cursor = tokenize(text_base, \" \"); for token in cursor {{ print(token); }} return 0; }}");
        let source = format!("{source} fn local_words() -> i64 {{ let pieces = split(\"a,b\", \",\"); for piece in pieces {{ print(piece); }} return 0; }}");
        let source = format!("{source} enum Reading {{ Value(i64), Empty }} fn read_match(input: Reading) -> i64 {{ match input {{ Reading::Value(value) => {{ return value; }} Reading::Empty => {{ return 0; }} }} }}");
        let source = format!("{source} fn local_match() -> i64 {{ let reading = Reading::Value(42); match reading {{ Reading::Value(value) => {{ return value; }} Reading::Empty => {{ return 0; }} }} }}");
        let source = format!("{source} fn exits() -> i64 {{ exit(7); return 0; }} fn bad_exit() -> i64 {{ exit(false); return 0; }}");
        let source = format!("{source} enum Maybe {{ Some(i64), None }} fn step() -> Maybe {{ return Maybe::None; }} fn propagate() -> Maybe {{ step()?; return Maybe::Some(7); }} fn bind() -> Maybe {{ let value = step()?; return Maybe::Some(value); }}");
        let source = source.as_str();
        let tokens = Rc::new(ae_syntax::lex_diagnostic(source).unwrap());
        let print_token = tokens.partition_point(|token| token.offset < source.find("print(flag)").unwrap());
                let condition_token = tokens.partition_point(|token| token.offset < source.find("if flag").unwrap() + 3);
        let binding = source.find("flag").unwrap();
        let mut functions = SharedFunctions::default();
        functions.insert("compute".into(), Signature {
            id: 9, params: vec![Type::Int(IntTy::I64)], ret: Type::Int(IntTy::I64), file: "case.ae".into(),
            public: false, extern_c: false,
        });
        functions.insert("consume_point".into(), Signature {
            id: 12, params: vec![Type::Struct(0)], ret: Type::Int(IntTy::I64), file: "case.ae".into(),
            public: false, extern_c: false,
        });
        functions.insert("imported_scalar".into(), Signature {
            id: 10, params: vec![Type::Int(IntTy::I64)], ret: Type::Int(IntTy::I64), file: "imported.ae".into(),
            public: true, extern_c: false,
        });
        functions.insert("imported_private".into(), Signature {
            id: 11, params: vec![Type::Int(IntTy::I64)], ret: Type::Int(IntTy::I64), file: "imported.ae".into(),
            public: false, extern_c: false,
        });
        let mut structs = vec![
            StructDef { name: "Point".into(), fields: vec![("x".into(), Type::Int(IntTy::I64)), ("y".into(), Type::Int(IntTy::I64))], file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: None, drop_returns_unit: false },
            StructDef { name: "Inner".into(), fields: vec![("x".into(), Type::Int(IntTy::I64))], file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: None, drop_returns_unit: false },
            StructDef { name: "Outer".into(), fields: vec![("inner".into(), Type::Struct(1)), ("tag".into(), Type::Int(IntTy::I64))], file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: None, drop_returns_unit: false },
            StructDef { name: "Holder".into(), fields: vec![("ready".into(), Type::Bool), ("text".into(), Type::String)], file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: None, drop_returns_unit: false },
            StructDef { name: "Guard".into(), fields: vec![("id".into(), Type::Int(IntTy::I64))], file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: Some(0), drop_returns_unit: false },
        ];
        let mut struct_names = HashMap::new();
        struct_names.insert("Point".into(), 0);
        struct_names.insert("Holder".into(), 3);
        struct_names.insert("Inner".into(), 1);
        struct_names.insert("Outer".into(), 2);
        struct_names.insert("Guard".into(), 4);
        let mut enums = vec![EnumDef {
            name: "Reading", variants: vec![("Value".into(), vec![Type::Int(IntTy::I64)]), ("Empty".into(), vec![])],
            file: "case.ae".into(), public: false, pattern: "Reading".into(), aliases: Vec::new(), is_copy: false,
        }];
        let mut enum_names = HashMap::new();
        enum_names.insert("Reading".into(), 0);
        enums.push(EnumDef {
            name: "Maybe", variants: vec![("Some".into(), vec![Type::Int(IntTy::I64)]), ("None".into(), vec![])],
            file: "case.ae".into(), public: false, pattern: "Maybe".into(), aliases: Vec::new(), is_copy: false,
        });
        enum_names.insert("Maybe".into(), 1);
        functions.insert("step".into(), Signature {
            id: 19, params: vec![], ret: Type::Enum("Maybe", 1), file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let mut generics = Generics::default();
        let mut ctx = CompileContext::new();
        let mut parser = make_parser(
            "case.ae", source, tokens, &mut functions, &mut structs,
            &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut ctx,
        );
        let next_var = parser.next_var;
        let input = source.find("input: Reading").unwrap();
        parser.names.insert("input".into(), (89, false, Type::Enum("Reading", 0)));
        parser.binding_ranges.insert(89, input..input + "input".len());
        parser.seek_scalar_offset(source.find("{ match input").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved enum match").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Match(89, Type::Enum(_, 0), arms)]
            if matches!(arms[0].body.as_slice(), [Statement::Return(Expr::Variable(slot, Type::Int(IntTy::I64), ..), _)]
                if *slot == arms[0].bindings[0].0)));
        parser.names.remove("input");
        parser.binding_ranges.remove(&89);
        parser.seek_scalar_offset(source.find("{ exit(7)").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved exit").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Exit(Expr::Number(7), _), Statement::Return(..)]));
        parser.seek_scalar_offset(source.find("{ exit(false)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved exit diagnostic").err().unwrap();
        assert_eq!(error.message, "exit expects an i64 status code");
        assert_eq!(*parser.current(), Kind::RParen);
        parser.return_type = Type::Enum("Maybe", 1);
        parser.seek_scalar_offset(source.find("{ step()?").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved bare propagation").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Propagate(Expr::Call(19, _, Type::Enum(_, 1)), Type::Enum(_, 1), _, None, _),
            Statement::Return(Expr::EnumLit(_, 1, 0, _), _)]));
        parser.seek_scalar_offset(source.find("{ let value = step").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved binding propagation").unwrap();
        let [Statement::Propagate(prop_value, prop_ty, prop_fields, prop_bound, _), Statement::Return(ret_expr, _)] = body.as_slice() else {
            panic!("unexpected binding-propagation body: {body:?}");
        };
        assert!(matches!(prop_value, Expr::Call(19, args, Type::Enum(_, 1)) if args.is_empty()), "{prop_value:?}");
        assert_eq!(prop_ty, &Type::Enum("Maybe", 1));
        assert_eq!(prop_fields, &[Type::Int(IntTy::I64)]);
        let Some((payload_slot, payload_ty)) = prop_bound else {
            panic!("payload binding missing: {prop_bound:?}");
        };
        assert_eq!(payload_ty, &Type::Int(IntTy::I64));
        assert!(matches!(ret_expr, Expr::EnumLit(_, _, 0, vals)
            if matches!(&**vals, [Expr::Variable(vs, Type::Int(IntTy::I64), ..)] if vs == payload_slot)), "{ret_expr:?}");
        parser.return_type = Type::Int(IntTy::I64);
        parser.seek_scalar_offset(source.find("{ let reading =").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local enum match").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(slot, Expr::EnumLit(_, 0, 0, _)),
            Statement::Match(subject, Type::Enum(_, 0), arms), ..] if slot == subject && arms.len() == 2));
        parser.next_var = next_var;
        let start = source.find("raw_param:").unwrap();
        parser.names.insert("raw_param".into(), (85, false, Type::PtrMut(parser.ctx.intern_pointee(Type::Int(IntTy::I64)))));
        parser.binding_ranges.insert(85, start..start + "raw_param".len());
        parser.seek_scalar_offset(source.find("{ unsafe { *raw_param").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved pointer store").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Unsafe(inner), Statement::Return(..)]
            if matches!(inner.as_slice(), [Statement::PtrStore(85, Expr::Number(7), ..)])));
        parser.names.remove("raw_param");
        parser.binding_ranges.remove(&85);
        let start = source.find("read_param:").unwrap();
        parser.names.insert("read_param".into(), (86, false, Type::Ptr(parser.ctx.intern_pointee(Type::Int(IntTy::I64)))));
        parser.binding_ranges.insert(86, start..start + "read_param".len());
        parser.seek_scalar_offset(source.find("{ unsafe { return read_param").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved pointer read").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Unsafe(inner)]
            if matches!(inner.as_slice(), [Statement::Return(Expr::DerefPtr(..), _)])));
        parser.names.remove("read_param");
        parser.binding_ranges.remove(&86);
        parser.seek_scalar_offset(source.find("{ var pointee = 1").unwrap());
        let next_var = parser.next_var;
        let body = parser.lower_resolved_scalar_body().expect("resolved local pointer body").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(_, Expr::Number(1)), Statement::Declare(pointer, Expr::BorrowAs(_, true, ..)), Statement::Unsafe(inner)]
            if matches!(inner.as_slice(), [Statement::PtrStore(slot, Expr::Number(7), ..), Statement::Return(Expr::DerefPtr(..), _)] if slot == pointer)));
        parser.next_var = next_var;
        let start = source.find("vector_arg:").unwrap();
        let vec_f64_ref = Type::VectorRef(parser.ctx.intern_vector_elem(Type::Float(FloatTy::F64)));
        parser.names.insert("vector_arg".into(), (87, false, vec_f64_ref));
        parser.binding_ranges.insert(87, start..start + "vector_arg".len());
        parser.return_type = Type::Float(FloatTy::F64);
        parser.seek_scalar_offset(source.find("{ var total = 0.0").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved vector loop").unwrap();
        assert!(matches!(body.as_slice(), [_, _, _, Statement::For(_, _, inner, ..), Statement::Return(..)]
            if matches!(inner.first(), Some(Statement::Declare(_, Expr::VecIndex(87, _, _, _, true, Type::Float(FloatTy::F64)))))));
        parser.return_type = Type::Int(IntTy::I64);
        parser.names.remove("vector_arg");
        parser.binding_ranges.remove(&87);
        parser.seek_scalar_offset(source.find("{ let empty_vec =").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved owning vector loop").unwrap();
        let vec_i64 = Type::Vector(parser.ctx.intern_vector_elem(Type::Int(IntTy::I64)));
        assert!(matches!(body.as_slice(), [Statement::Declare(vector, Expr::VecLit(values, ty, ..)), _, _, Statement::For(_, _, inner, ..), Statement::Return(_, owned), Statement::Frees(_)]
            if *ty == vec_i64 && values.is_empty() && owned.contains(&(*vector, vec_i64))
                && matches!(inner.first(), Some(Statement::Declare(_, Expr::VecIndex(source, _, _, _, false, Type::Int(IntTy::I64)))) if source == vector)));
        parser.seek_scalar_offset(source.find("{ spawn(").unwrap());
        // A spawn statement still keeps the body on the typed route; the
        // A bare `spawn(..)` statement is evaluated (the spawn machinery) and
        // then rejected with the typed route's `?` rule; the path leaves no
        // move state behind.
        let error = parser.lower_resolved_scalar_body().expect("resolved spawn statement").err().unwrap();
        assert!(error.message.contains("must end with '?'"), "{}", error.message);
        let start = source.find("words: &vec[string]").unwrap();
        let vec_str_ref = Type::VectorRef(parser.ctx.intern_vector_elem(Type::String));
        parser.names.insert("words".into(), (88, false, vec_str_ref));
        parser.binding_ranges.insert(88, start..start + "words".len());
        parser.seek_scalar_offset(source.find("{ for word in words").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved string vector loop").unwrap();
        assert!(matches!(body.as_slice(), [_, _, Statement::For(_, _, inner, ..), Statement::Return(..)]
            if matches!(inner.as_slice(), [Statement::Declare(slot, Expr::StrVecIndex(88, _, _, _, true)),
                Statement::Print(Printed::Expr(Expr::Variable(read, Type::StrView, ..)))] if slot == read)));
        parser.names.remove("words");
        parser.binding_ranges.remove(&88);
        parser.seek_scalar_offset(source.find("{ let text_base").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved token loop").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(base, Expr::StrLit(..)),
            Statement::Declare(cursor, Expr::Tokens(source, None, ..)), Statement::ForTokens(read, ..), ..]
            if base == source && cursor == read));
        assert!(parser.borrows.is_empty());
        parser.seek_scalar_offset(source.find("{ let pieces = split").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local string vector loop").unwrap();
        let vec_str = Type::Vector(parser.ctx.intern_vector_elem(Type::String));
        assert!(matches!(body.as_slice(), [Statement::Declare(vector, Expr::Split(..)), _, _, Statement::For(_, _, inner, ..), Statement::Return(_, owned), Statement::Frees(_)]
            if owned.contains(&(*vector, vec_str))
                && matches!(inner.first(), Some(Statement::Declare(_, Expr::StrVecIndex(source, _, _, _, false))) if source == vector)));
        parser.next_var = next_var;
        parser.next_label = 0;
        parser.generics.names.insert("CachedBox".into(), 0);
        parser.generics.defs.push(GenericEnum {
            name: "CachedBox".into(), params: vec!["T".into()],
            variants: vec![("Item".into(), vec![Payload::Param(0)])],
            file: "case.ae".into(), public: false, is_copy: false,
        });
        let payload = source.find("specialized_text:").unwrap();
        parser.names.insert("specialized_text".into(), (82, false, Type::String));
        parser.binding_ranges.insert(82, payload..payload + "specialized_text".len());
        let variant = source.find("CachedBox[string]::Item").unwrap();
        parser.seek_scalar_offset(variant);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&82));
        let eid = parser.instantiate_enum(0, vec![Type::String], variant).unwrap();
        parser.seek_scalar_offset(variant);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::EnumLit(_, actual, 0, payload), Type::Enum(_, ty)))
                if actual == eid && ty == eid && matches!(payload.as_slice(), [Expr::Variable(82, Type::String, ..)])
        ));
        assert!(parser.moved.remove(&82));
        parser.names.remove("specialized_text");
        parser.binding_ranges.remove(&82);
        parser.generics.struct_names.insert("CachedRecord".into(), 0);
        parser.generics.struct_defs.push(GenericStruct {
            name: "CachedRecord".into(), params: vec!["T".into()],
            fields: vec![("value".into(), Payload::Param(0)), ("count".into(), Payload::Concrete(Type::Int(IntTy::I64)))],
            file: "case.ae".into(), public: false, is_copy: false,
        });
        let payload = source.find("record_text:").unwrap();
        parser.names.insert("record_text".into(), (83, false, Type::String));
        parser.binding_ranges.insert(83, payload..payload + "record_text".len());
        let constructor = source.find("CachedRecord[string] { count").unwrap();
        parser.seek_scalar_offset(constructor);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&83));
        let sid = parser.instantiate_struct(0, vec![Type::String], constructor).unwrap();
        parser.seek_scalar_offset(constructor);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::StructLit(actual, fields), Type::Struct(ty)))
                if actual == sid && ty == sid
                    && matches!(fields.as_slice(), [Expr::Variable(83, Type::String, ..), Expr::Number(2)])
        ));
        assert!(parser.moved.remove(&83));
        parser.names.remove("record_text");
        parser.binding_ranges.remove(&83);
        let definition = source.find("(generic_value: T)").unwrap();
        parser.generics.fn_names.insert("generic_scalar".into(), 0);
        parser.generics.fn_defs.push(GenericFn {
            name: "generic_scalar".into(), bindings: None, params: vec!["T".into()],
            bounds: vec![vec![]], trait_bounds: vec![vec![]],
            start: parser.tokens.partition_point(|token| token.offset < definition),
            tokens: Rc::clone(&parser.tokens), source: Rc::clone(&parser.shared_source),
            file: "case.ae".into(), public: false,
        });
        let payload = source.find("call_text:").unwrap();
        parser.names.insert("call_text".into(), (84, false, Type::String));
        parser.binding_ranges.insert(84, payload..payload + "call_text".len());
        let call = source.find("generic_scalar[string](call_text)").unwrap();
        parser.seek_scalar_offset(call);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&84));
        let (id, params, _) = parser.instantiate_fn(0, vec![Type::String], call).unwrap();
        let _ = &params;
        parser.seek_scalar_offset(call);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::Call(actual, arguments, Type::String), Type::String))
                if actual == id && matches!(arguments.as_slice(), [Argument::Value(Expr::Variable(84, Type::String, ..))])
        ));
        assert!(parser.moved.remove(&84));
        parser.names.remove("call_text");
        parser.binding_ranges.remove(&84);
        let mut imported = parser.generics.fn_defs[0].clone();
        imported.name = "imported_generic".into();
        imported.file = "imported.ae".into();
        imported.public = true;
        parser.generics.fn_names.insert(imported.name.clone(), 1);
        parser.generics.fn_defs.push(imported);
        let offset = source.find("imported_generic[i64](7)").unwrap();
        let (id, _, _) = parser.instantiate_fn(1, vec![Type::Int(IntTy::I64)], offset).unwrap();
        parser.seek_scalar_offset(offset);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), false),
            Some((Expr::Call(actual, _, Type::Int(IntTy::I64)), Type::Int(IntTy::I64))) if actual == id
        ));
        parser.generics.fn_defs[1].public = false;
        parser.seek_scalar_offset(offset);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, start);
        let capture_start = source.find("captured_scalar:").unwrap();
        parser.names.insert("captured_scalar".into(), (80, true, Type::Int(IntTy::I64)));
        parser.seek_scalar_offset(source.find("{ return captured_scalar;").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(80, capture_start)])
            .expect("resolved emitted closure body").unwrap();
        assert!(matches!(closure.as_slice(), [Statement::Return(Expr::Variable(80, Type::Int(IntTy::I64), ..), _)]));
        parser.names.remove("captured_scalar");
        // A captured owned string flows through the capture slot: the
        // resolver records the capture and the route re-admits its read.
        let keep_start = source.find("let keep =").unwrap() + 4;
        parser.names.insert("keep".into(), (82, false, Type::StrView));
        parser.seek_scalar_offset(source.find("|| -> i64 { print(keep)").unwrap() + 10);
        let closure = parser.lower_resolved_body_with_captures(&[(82, keep_start)])
            .expect("resolved string-capture closure body").unwrap();
        assert!(matches!(closure.as_slice(), [Statement::Print(Printed::Expr(Expr::Variable(82, Type::StrView, ..))),
            Statement::Return(Expr::Number(0), _)]));
        // A captured owned string is the value-capture (FnOnce) shape: the read
        // lowers, and the capture is released unless the body moved it out.
        parser.names.insert("keep".into(), (82, false, Type::String));
        parser.seek_scalar_offset(source.find("{ print(keep)").unwrap());
        let captured = parser.lower_resolved_body_with_captures(&[(82, keep_start)])
            .expect("resolved owned-string capture").unwrap();
        assert!(matches!(captured.first(),
            Some(Statement::Print(Printed::Expr(Expr::Variable(82, Type::String, ..))))));
        parser.names.remove("keep");
        let numbers_start = source.find("let numbers = vec[10, 20]").unwrap() + 4;
        let vector_body = source.find("{ print(numbers[1])").unwrap();
        let vec_i64_ref = Type::VectorRef(parser.ctx.intern_vector_elem(Type::Int(IntTy::I64)));
        let vec_f64_ref = Type::VectorRef(parser.ctx.intern_vector_elem(Type::Float(FloatTy::F64)));
        for (capture_type, element) in [(vec_i64_ref, Type::Int(IntTy::I64)), (vec_f64_ref, Type::Float(FloatTy::F64))] {
            parser.names.insert("numbers".into(), (82, false, capture_type));
            parser.seek_scalar_offset(vector_body);
            let closure = parser.lower_resolved_body_with_captures(&[(82, numbers_start)])
                .expect("resolved numeric vector-capture closure body").unwrap();
            assert!(matches!(closure.as_slice(),
                [Statement::Print(Printed::Expr(Expr::VecIndex(82, _, _, _, true, actual))),
                 Statement::Return(Expr::Number(0), _)] if *actual == element));
        }
        // An owning value capture moves the vector into the closure's slot;
        // the element read is a borrowed index into that slot, and the
        // body's own Frees releases the slot after the call (FnOnce).
        let vec_i64 = Type::Vector(parser.ctx.intern_vector_elem(Type::Int(IntTy::I64)));
        parser.names.insert("numbers".into(), (82, false, vec_i64));
        parser.seek_scalar_offset(vector_body);
        let closure = parser.lower_resolved_body_with_captures(&[(82, numbers_start)])
            .expect("resolved owning numeric vector-capture closure body").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::VecIndex(82, _, _, _, false, Type::Int(IntTy::I64)))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("numbers");
        // A borrowed string-vector capture: its element read is a view.
        let labels_start = source.find("let labels = vec[").unwrap() + 4;
        let vec_str_ref = Type::VectorRef(parser.ctx.intern_vector_elem(Type::String));
        parser.names.insert("labels".into(), (83, false, vec_str_ref));
        parser.seek_scalar_offset(source.find("{ print(labels[0])").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(83, labels_start)])
            .expect("resolved borrowed string-vector capture").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::StrVecIndex(83, _, _, _, true))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("labels");
        // An owning string-vector capture: the element read comes from the
        // closure's own slot (by_ref=false). The caller (closure_body)
        // appends the scope's Frees, which releases the vector and its
        // elements after the call.
        let vec_str = Type::Vector(parser.ctx.intern_vector_elem(Type::String));
        parser.names.insert("labels".into(), (84, false, vec_str));
        parser.seek_scalar_offset(source.find("{ print(labels[0])").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(84, labels_start)])
            .expect("resolved owning string-vector capture").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::StrVecIndex(84, _, _, _, false))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("labels");
        let generic_binding = source.find("generic_value:").unwrap();
        let body = source.find("{ return generic_value;").unwrap();
        for ty in [Type::Int(IntTy::I64), Type::Bool] {
            parser.subst.insert("T".into(), ty);
            parser.return_type = ty;
            parser.names.insert("generic_value".into(), (80, false, ty));
            parser.binding_ranges.insert(80, generic_binding..generic_binding + "generic_value".len());
            parser.seek_scalar_offset(body);
            let lowered = parser.lower_resolved_scalar_body().expect("resolved generic instance").unwrap();
            assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Variable(80, actual, ..), _)] if *actual == ty));
        }
        parser.names.remove("generic_value");
        let method_binding = source.find("method_value:").unwrap();
        parser.return_type = Type::Int(IntTy::I64);
        parser.subst.insert("T".into(), Type::Int(IntTy::I64));
        parser.impl_self = Some(Type::Struct(0));
        parser.names.insert("method_value".into(), (80, false, Type::Int(IntTy::I64)));
        parser.binding_ranges.insert(80, method_binding..method_binding + "method_value".len());
        parser.seek_scalar_offset(source.find("{ return method_value;").unwrap());
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic associated body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Variable(80, Type::Int(IntTy::I64), ..), _)]));
        parser.names.remove("method_value");
        let receiver_binding = source.find("get(&self)").unwrap() + "get(&".len();
        let instance = parser.structs.len();
        parser.structs.push(StructDef {
            name: "ScalarBox[i64]".into(), fields: vec![("item".into(), Type::Int(IntTy::I64))],
            file: "case.ae".into(), public: false, repr_c: false, is_copy: false, drop_fn: None, drop_returns_unit: false,
        });
        let receiver = Type::StructRef(instance);
        parser.receiver = Some(receiver);
        parser.names.insert("self".into(), (80, false, receiver));
        parser.binding_ranges.insert(80, receiver_binding..receiver_binding + "self".len());
        let receiver_body = source.find("{ return self.item;").unwrap();
        parser.seek_scalar_offset(receiver_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic receiver body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Field(base, name, Type::Int(IntTy::I64)), _)]
            if name == "item" && matches!(**base, Expr::Variable(80, actual, ..) if actual == receiver)));
        let setter_binding = source.find("set(&mut self").unwrap() + "set(&mut ".len();
        let value_binding = source.find("new_value:").unwrap();
        let setter_body = source.find("{ self.item = new_value;").unwrap();
        let mutable_receiver = Type::StructMut(instance);
        parser.receiver = Some(mutable_receiver);
        parser.names.insert("self".into(), (80, true, mutable_receiver));
        parser.names.insert("new_value".into(), (81, false, Type::Int(IntTy::I64)));
        parser.binding_ranges.insert(80, setter_binding..setter_binding + "self".len());
        parser.binding_ranges.insert(81, value_binding..value_binding + "new_value".len());
        parser.seek_scalar_offset(setter_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic receiver store").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::FieldStore(80, name, Type::Int(IntTy::I64), Expr::Variable(81, Type::Int(IntTy::I64), ..), actual), Statement::Return(Expr::Field(..), _)]
            if name == "item" && *actual == mutable_receiver));
        parser.names.insert("new_value".into(), (81, false, Type::Bool));
        parser.seek_scalar_offset(setter_body);
        let error = parser.lower_resolved_scalar_body().expect("resolved setter error").err().unwrap();
        assert_eq!(error.message, "field 'item' must be i64");
        assert_eq!(*parser.current(), Kind::Return);
        parser.names.remove("new_value");
        parser.binding_ranges.remove(&81);
        let owned_binding = source.find("consume_box(self)").unwrap() + "consume_box(".len();
        let owned_body = source.find("consume_box(self) -> T {").unwrap() + "consume_box(self) -> T ".len();
        let owned_receiver = Type::Struct(instance);
        parser.receiver = Some(owned_receiver);
        parser.names.insert("self".into(), (80, false, owned_receiver));
        parser.binding_ranges.insert(80, owned_binding..owned_binding + "self".len());
        parser.scopes.push(vec![(80, owned_receiver)]);
        parser.seek_scalar_offset(owned_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved owned receiver body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Field(..), owned)]
            if owned == &[(80, owned_receiver)]));
        parser.scopes.pop();
        let transfer_binding = source.find("round_trip(self,").unwrap() + "round_trip(".len();
        let route_binding = source.find("route:").unwrap();
        parser.return_type = owned_receiver;
        parser.binding_ranges.insert(80, transfer_binding..transfer_binding + "self".len());
        parser.names.insert("route".into(), (81, false, Type::Bool));
        parser.binding_ranges.insert(81, route_binding..route_binding + "route".len());
        parser.scopes.push(vec![(80, owned_receiver)]);
        parser.seek_scalar_offset(source.find("{ if route").unwrap());
        let lowered = parser.lower_resolved_scalar_body().expect("resolved owning transfer").unwrap();
        let [Statement::If(_, yes, no)] = lowered.as_slice() else { panic!("expected both return branches") };
        for branch in [yes, no] {
            assert!(matches!(branch.as_slice(), [Statement::Return(Expr::Variable(80, ty, ..), owned)]
                if *ty == owned_receiver && owned == &[(80, owned_receiver)]));
        }
        assert!(parser.moved.remove(&80));
        parser.scopes.pop();
        parser.names.remove("route");
        parser.binding_ranges.remove(&81);
        parser.return_type = Type::Int(IntTy::I64);
        parser.functions.insert("take_box".into(), Signature {
            id: 13, params: vec![owned_receiver], ret: Type::Int(IntTy::I64), file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let send_binding = source.find("send(self)").unwrap() + "send(".len();
        parser.binding_ranges.insert(80, send_binding..send_binding + "self".len());
        parser.seek_scalar_offset(source.find("{ return take_box((self));").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved receiver argument").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Call(13, args, Type::Int(IntTy::I64)), _)]
            if matches!(args.as_slice(), [Argument::Value(Expr::Variable(80, ty, ..))] if *ty == owned_receiver)));
        assert!(parser.moved.remove(&80));
        let twice_binding = source.find("send_twice(self)").unwrap() + "send_twice(".len();
        let next_slot = parser.next_var;
        parser.binding_ranges.insert(80, twice_binding..twice_binding + "self".len());
        parser.seek_scalar_offset(source.find("{ let sent = take_box(self)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved moved receiver").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0102"));
        parser.moved.remove(&80);
        parser.names.remove("sent");
        parser.binding_ranges.remove(&next_slot);
        parser.next_var = next_slot;
        parser.receiver = Some(receiver);
        parser.names.insert("self".into(), (80, false, receiver));
        parser.binding_ranges.insert(80, receiver_binding..receiver_binding + "self".len());
        parser.structs[instance].fields[0].1 = Type::String;
        parser.seek_scalar_offset(receiver_body);
        // A string receiver field reads as a borrowed view on this route: the
        // body is taken (the field read lowers, or the shape check answers)
        // rather than handed back whole to the typed parser.
        assert!(parser.lower_resolved_scalar_body().is_some(), "a string receiver field no longer forces the typed route");
        parser.structs.pop();
        parser.receiver = None;
        parser.names.remove("self");
        parser.subst.clear();
        parser.impl_self = None;
        parser.binding_ranges.remove(&80);
        parser.return_type = Type::Bool;
        let comparison = "local == 20";
        let start = source.find(comparison).unwrap();
        assert_eq!(
            parser.resolved_expressions().scalar_expression_types.get(&(start..start + comparison.len())),
            Some(&ae_resolve::resolve::ExpressionType::Checked(ae_ty::Scalar::Bool)),
        );
        parser.seek_scalar_offset(source.find("{ let local").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved scalar route").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(0, _), Statement::Return(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), _)]));
        parser.return_type = Type::Int(IntTy::I64);
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let mismatch = parser.lower_resolved_scalar_body().expect("resolved error route").err().unwrap();
        assert_eq!(mismatch.message, "return type must be i64");
        assert_eq!(mismatch.code.as_deref(), Some("E0204"));

        parser.seek_scalar_offset(source.find("{ if false").unwrap());
        let branches = parser.lower_resolved_scalar_body().expect("resolved branches").unwrap();
        let [Statement::If(_, yes, otherwise)] = branches.as_slice() else { panic!("expected if") };
        let [Statement::If(_, other_yes, other_no)] = otherwise.as_slice() else { panic!("expected else if") };
        for (index, body) in [yes, other_yes, other_no].into_iter().enumerate() {
            assert!(matches!(body.as_slice(), [Statement::Declare(slot, _), Statement::Return(Expr::Variable(read, Type::Int(IntTy::I64), ..), _)] if *slot == index + 1 && slot == read));
        }

        parser.seek_scalar_offset(source.find("{ while true").unwrap());
        let loops = parser.lower_resolved_scalar_body().expect("resolved loops").unwrap();
        let [Statement::While(_, outer), Statement::Return(Expr::Number(7), _)] = loops.as_slice() else {
            panic!("expected loop and return");
        };
        let [Statement::While(_, inner), Statement::Break] = outer.as_slice() else {
            panic!("expected nested loop and break");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        parser.seek_scalar_offset(source.find("{ continue; return").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved loop error").err().unwrap();
        assert_eq!(error.message, "'continue' outside of a loop");
        assert_eq!(parser.loop_depth, 0);

        parser.seek_scalar_offset(source.find("{ for i in 2").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved range").unwrap();
        let [Statement::Declare(start, Expr::Number(2)), Statement::Declare(end, Expr::Number(5)), Statement::For(counter, limit, loop_body, label, ..), Statement::Declare(reused, Expr::Number(7)), Statement::Return(Expr::Variable(read, Type::Int(IntTy::I64), ..), _)] = body.as_slice() else {
            panic!("expected range bounds and loop");
        };
        assert_eq!((start, end), (counter, limit));
        assert_eq!(*start, *end + 1);
        assert_ne!(counter, reused);
        assert_eq!(reused, read);
        let [Statement::While(_, inner), Statement::If(_, branch, _)] = loop_body.as_slice() else {
            panic!("expected nested while and conditional");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        assert!(matches!(branch.as_slice(), [Statement::Continue(Some(jump))] if jump == label));

        parser.seek_scalar_offset(source.find("{ for j in false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved range error").err().unwrap();
        assert_eq!(error.message, "range bounds must be i64");

        parser.seek_scalar_offset(source.find("{ var n = 4").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved assignments").unwrap();
        let [Statement::Declare(slot, _), Statement::Assign(plain, Expr::Number(2), false), Statement::While(_, loop_body), Statement::Return(Expr::Variable(read, Type::Int(IntTy::I64), ..), _)] = body.as_slice() else {
            panic!("expected scalar assignments");
        };
        assert_eq!(slot, plain);
        assert_eq!(slot, read);
        assert!(matches!(loop_body.as_slice(), [Statement::Assign(compound, Expr::Binary(_, Kind::Minus, _, _, _, Type::Int(IntTy::I64)), false)] if compound == slot));

        parser.seek_scalar_offset(source.find("{ let n = 1").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable assignment").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'n'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("n");

        parser.seek_scalar_offset(source.find("{ print(2);").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved output").unwrap();
        let [Statement::Print(Printed::Expr(Expr::Number(2))), Statement::Assert(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), 1, column), Statement::Return(Expr::Number(0), _)] = body.as_slice() else {
            panic!("expected print, assert, and return");
        };
        assert_eq!(*column, source.find("assert(2 == 2)").unwrap() + 1);
        parser.seek_scalar_offset(source.find("{ assert(5)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved assert error").err().unwrap();
        assert_eq!(error.message, "assert requires a boolean condition");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.in_main = true;
        parser.seek_scalar_offset(source.find("{ print(23)").unwrap());
        let entry = parser.lower_resolved_scalar_body().expect("resolved entry body").unwrap();
        assert!(matches!(entry.as_slice(), [Statement::Print(Printed::Expr(Expr::Number(23)))]));
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved entry return error").err().unwrap();
        assert_eq!(error.message, "return is not supported in main");
        assert_eq!(*parser.current(), Kind::Return);
        parser.in_main = false;

        parser.seek_scalar_offset(source.find("{ return compute(compute").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved direct calls").unwrap();
        let [Statement::Return(Expr::Call(9, args, Type::Int(IntTy::I64)), _)] = body.as_slice() else {
            panic!("expected direct call");
        };
        assert!(matches!(args.as_slice(), [Argument::Value(Expr::Call(9, _, Type::Int(IntTy::I64)))]));
        parser.seek_scalar_offset(source.find("{ return compute(false)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved call error").err().unwrap();
        assert_eq!(error.message, "argument 1 to 'compute' must be i64");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.seek_scalar_offset(source.find("{ return imported_scalar").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved cross-module call").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Call(10, _, Type::Int(IntTy::I64)), _)]));
        parser.seek_scalar_offset(source.find("{ return imported_private").unwrap());
        // A private import is admitted by the scan; the lowering reports the
        // typed route's visibility rule.
        let error = parser.lower_resolved_scalar_body().expect("resolved private import").err().unwrap();
        assert!(error.message.contains("is private to"), "{}", error.message);

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local array").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Return(Expr::Index(read, 2, index, ..), _)]
            if slot == read && values.len() == 2 && matches!(**index, Expr::Number(1))));
        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved array error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[1] = 9").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array store").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Store(read, 2, store_index, element, ..), Statement::Return(Expr::Index(read_back, 2, back_index, ..), _)]
            if slot == read && read == read_back && values.len() == 2
                && matches!(store_index, Expr::Number(1)) && matches!(element, Expr::Number(9))
                && matches!(**back_index, Expr::Number(1))));
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store index error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; a[0] = 9").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] = false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store value error").err().unwrap();
        assert_eq!(error.message, "array elements must be i64");
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] += 1").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none(), "compound store stays typed");

        let param_offset = source.find("(a: &[i64; 2]").unwrap() + 1;
        parser.names.insert("a".into(), (0, false, Type::Array(2)));
        parser.binding_ranges.insert(0, param_offset..param_offset + 1);
        parser.seek_scalar_offset(source.find("{ return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array param").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Index(slot, 2, index, ..), _)]
            if *slot == 0 && matches!(**index, Expr::Number(1))));
        let store_param_offset = source[param_offset + 1..].find("(a: &[i64; 2]").unwrap() + param_offset + 2;
        parser.binding_ranges.insert(0, store_param_offset..store_param_offset + 1);
        parser.seek_scalar_offset(source.find("{ a[0] = 3").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved param store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        let p_offset = source.find("(p: &Point)").unwrap() + 1;
        parser.names.insert("p".into(), (0, false, Type::StructRef(0)));
        parser.binding_ranges.insert(0, p_offset..p_offset + 1);
        parser.seek_scalar_offset(source.find("{ return p.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved field reads").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Binary(lhs, Kind::Plus, rhs, ..), _)]
            if matches!(&**lhs, Expr::Field(base, fname, fty)
                if matches!(&**base, Expr::Variable(0, Type::StructRef(0), ..)) && fname == "x" && *fty == Type::Int(IntTy::I64))
                && matches!(&**rhs, Expr::Field(_, fname, fty) if fname == "y" && *fty == Type::Int(IntTy::I64))));
        parser.names.remove("p");

        let h_offset = source.find("(h: &Holder)").unwrap() + 1;
        parser.names.insert("h".into(), (0, false, Type::StructRef(3)));
        parser.binding_ranges.insert(0, h_offset..h_offset + 1);
        parser.seek_scalar_offset(source.find("{ return h.text").unwrap());
        // A string field read is a view, but returning it would move it out of
        // the borrowed struct: the route takes the body and rejects the move.
        let error = parser.lower_resolved_scalar_body().expect("string field return").err().unwrap();
        assert!(error.message.contains("cannot move a field out of a struct"), "{}", error.message);
        parser.names.remove("h");

        let o_offset = source.find("(o: &Outer)").unwrap() + 1;
        parser.names.insert("o".into(), (0, false, Type::StructRef(2)));
        parser.binding_ranges.insert(0, o_offset..o_offset + 1);
        parser.seek_scalar_offset(source.find("{ return o.inner.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved nested field read").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Field(outer, fname, fty), _)]
            if matches!(&**outer, Expr::Field(inner, iname, ifty)
                if matches!(&**inner, Expr::Variable(0, Type::StructRef(2), ..)) && iname == "inner" && *ifty == Type::Struct(1))
                && fname == "x" && *fty == Type::Int(IntTy::I64)));
        parser.names.remove("o");

        parser.seek_scalar_offset(source.find("{ let p = Point { x: 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved construction").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(slot, Expr::StructLit(sid, values)),
             Statement::Return(Expr::Binary(l, Kind::Plus, r, ..), frees),
             Statement::Frees(scope_frees)]
            if *sid == 0 && values.len() == 2
                && matches!(&**l, Expr::Field(b, f, t)
                    if f == "x" && *t == Type::Int(IntTy::I64)
                        && matches!(&**b, Expr::Variable(ps, Type::Struct(0), ..) if *ps == *slot))
                && matches!(&**r, Expr::Field(_, f2, t2) if f2 == "y" && *t2 == Type::Int(IntTy::I64))
                && *frees == vec![(*slot, Type::Struct(0))]
                && *scope_frees == vec![(*slot, Type::Struct(0))]));
        parser.names.remove("p");

        parser.seek_scalar_offset(source.find("{ let g = Guard { id: 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved drop construction").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(slot, Expr::StructLit(sid, _)),
             Statement::Return(Expr::Field(_, fname, fty), frees),
             Statement::Frees(scope_frees)]
            if *sid == 4 && fname == "id" && *fty == Type::Int(IntTy::I64)
                && *frees == vec![(*slot, Type::Struct(4))]
                && *scope_frees == vec![(*slot, Type::Struct(4))]),
            "a drop-carrying construction is owned and released on every exit path");

        parser.seek_scalar_offset(source.find("{ let wrong = Point").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved constructor field error").err().unwrap();
        assert_eq!(error.message, "field 'x' must be i64");
        assert_eq!(*parser.current(), Kind::Comma);
        for (binding, message) in [
            ("{ let extra", "struct 'Point' has no field 'z'"),
            ("{ let dup", "field 'x' specified twice"),
            ("{ let missing", "missing field 'y' in 'Point' construction"),
        ] {
            parser.seek_scalar_offset(source.find(binding).unwrap());
            let error = parser.lower_resolved_scalar_body().expect("resolved constructor shape error").err().unwrap();
            assert_eq!(error.message, message);
        }
        parser.names.remove("g");

        parser.seek_scalar_offset(source.find("{ let q = Point { x: 3").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved owned-aggregate call").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(q, Expr::StructLit(0, values)),
             Statement::Declare(r, Expr::Call(12, args, Type::Int(IntTy::I64))),
             Statement::Return(.., frees),
             Statement::Frees(scope_frees)]
            if values.len() == 2
                && matches!(args.as_slice(), [Argument::Value(Expr::Variable(qs, Type::Struct(0), ..))] if *qs == *q)
                && *frees == vec![(*q, Type::Struct(0))]
                && *scope_frees == vec![(*q, Type::Struct(0))]));
        parser.names.remove("q");
        parser.names.remove("r");

        parser.seek_scalar_offset(source.find("{ let q = Point { x: 5").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved use after move").err().unwrap();
        assert_eq!(error.message, "cannot use 'q' because it was moved");
        parser.names.remove("q");
        parser.names.remove("r");
        parser.names.remove("s");

        let b_offset = source.find("(b: bool)").unwrap() + 1;
        parser.names.insert("b".into(), (5, false, Type::Bool));
        parser.binding_ranges.insert(5, b_offset..b_offset + 1);
        parser.seek_scalar_offset(source.find("{ unsafe { let a = 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved unsafe block").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Unsafe(inner), Statement::Return(Expr::Number(2), frees)]
            if matches!(inner.as_slice(), [Statement::Declare(_slot, Expr::Number(1))])
                && frees.is_empty()));
        parser.seek_scalar_offset(source.find("{ let q = Point { x: 7").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved loop move").err().unwrap();
        assert!(error.message.contains("cannot move 'q' inside a loop body"));
        parser.names.remove("q");
        parser.names.remove("r");
        parser.names.remove("b");

        for prefix in [
            "choose[",
            "Pair[i64, vec[f64]] {",
            "Pair[i64, vec[f64]]::",
            "Choice[i64, vec[f64]]::Some",
            "Choice[i64, vec[f64]]::empty",
        ] {
            let target = source.find(prefix).unwrap();
            let ranges = parser.shared_specialization_arguments(target).unwrap();
            let spellings: Vec<_> = ranges.iter().map(|range| &source[range.clone()]).collect();
            assert_eq!(spellings, ["i64", "vec[f64]"]);
        }

        let target = source.find("*const shared").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < target);
        assert_eq!(parser.shared_pointer_cast_qualifier(), Some(false));
        assert_eq!(parser.pointer_cast_pointee(None).unwrap(), (false, Type::Shared(parser.ctx.intern_shared(Type::Int(IntTy::I64)))));
        assert_eq!(*parser.current(), Kind::Semi);

        for (prefix, expected) in [("|| -> i64", "i64"), ("|x: i64|", "bool")] {
            let offset = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < offset);
            let header = parser.shared_closure_header().unwrap();
            let ae_syntax::expression::ExpressionKind::Closure { params, return_type, .. } = &header.kind else {
                unreachable!("closure header index");
            };
            assert_eq!(&source[return_type.clone()], expected);
            if expected == "bool" {
                assert_eq!(params[0].name, "x");
                assert_eq!(&source[params[0].ty.clone()], "i64");
            } else {
                assert!(params.is_empty());
            }
        }
        let inline = source.find("task_spawn(ex, ||").unwrap() + "task_spawn(ex, ".len();
        parser.seek_scalar_offset(inline);
        assert!(matches!(parser.spawn_callable_with(None, &std::collections::HashMap::new()), Ok((Expr::ClosureLit(id), Type::Closure(ty))) if id == ty));
        assert_eq!(*parser.current(), Kind::RParen);

        parser.names.insert("flag".into(), (7, false, Type::Bool));
        parser.binding_ranges.insert(7, binding..binding + 4);

        parser.pos = print_token + 2;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, print_token + 2);

        parser.pos = condition_token;
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::LBrace), false),
            Some((Expr::Variable(7, Type::Bool, _, _), Type::Bool)),
        ));
        assert_eq!(*parser.current(), Kind::LBrace);


        let vector_offset = source.find("vec[1.0").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < vector_offset);
        let vec_f64 = Type::Vector(parser.ctx.intern_vector_elem(Type::Float(FloatTy::F64)));
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::VecLit(values, ty, _, _), ret_ty)) if ty == vec_f64 && ret_ty == vec_f64 && values.len() == 2
        ));
        let binding = source.find("var value").unwrap() + 4;
        parser.names.insert("value".into(), (12, true, Type::Bool));
        parser.binding_ranges.insert(12, binding..binding + 5);
        for (prefix, mutable) in [("&value", false), ("&mut value", true)] {
            let cast = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
            assert!(matches!(
                parser.lower_shared_value(Some(Kind::Semi), false),
                Some((Expr::BorrowAs(operand, actual, 1, column), ty))
                    if matches!(*operand, Expr::Variable(12, Type::Bool, _, _))
                        && actual == mutable && matches!(ty, Type::PtrMut(_)) == mutable
                        && column == cast + 1
            ));
            assert_eq!(*parser.current(), Kind::Semi);
        }

        parser.names.get_mut("value").unwrap().1 = false;
        let cast = source.find("&mut value").unwrap();
        let start = parser.tokens.partition_point(|token| token.offset < cast);
        parser.pos = start;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, start);

        let binding = source.find("let raw").unwrap() + 4;
        let raw = Type::Ptr(parser.ctx.intern_pointee(Type::Bool));
        parser.names.insert("raw".into(), (13, false, raw));
        parser.binding_ranges.insert(13, binding..binding + 3);
        let cast = source.find("raw as").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), false),
            Some((Expr::PtrCast(operand, true, id, _, column), Type::PtrMut(pointee)))
                if matches!(*operand, Expr::Variable(13, ty, _, _) if ty == raw)
                    && id == pointee && parser.ctx.ptr_pointee(id) == Type::Float(FloatTy::F64)
                    && column == cast + "raw as *mut f64;".len()
        ));
        assert_eq!(*parser.current(), Kind::Semi);
    }
