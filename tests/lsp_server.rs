use ae_compiler::lsp::{JsonValue, LineIndex, LspServer, Position};

#[test]
fn lsp_initialize_and_capabilities() {
    let mut server = LspServer::new();
    let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let resps = server.handle_message(init_req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    assert_eq!(val.get("id").and_then(|v| v.as_i64()), Some(1));
    let caps = val.get("result").and_then(|r| r.get("capabilities")).unwrap();
    assert_eq!(caps.get("textDocumentSync").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(caps.get("hoverProvider"), Some(&JsonValue::Bool(true)));
    assert_eq!(caps.get("definitionProvider"), Some(&JsonValue::Bool(true)));
}

#[test]
fn lsp_text_sync_with_multibyte_and_diagnostics() {
    let mut server = LspServer::new();
    let open_notif = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn main() {\n    let s = \"你好世界\";\n    print(s);\n}"}}}"#;
    let resps = server.handle_message(open_notif);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    assert_eq!(val.get("method").and_then(|m| m.as_str()), Some("textDocument/publishDiagnostics"));

    let change_invalid = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///test.ae","version":2},"contentChanges":[{"text":"fn main() {\n    let s = \"bad escape \\q\";\n}"}]}}"#;
    let resps2 = server.handle_message(change_invalid);
    assert_eq!(resps2.len(), 1);
    let val2 = JsonValue::parse(&resps2[0]).unwrap();
    let diags = val2.get("params").and_then(|p| p.get("diagnostics")).and_then(|d| d.as_array()).unwrap();
    assert!(!diags.is_empty());
}

#[test]
fn lsp_line_index_multibyte_offset_conversion() {
    let text = "a\n你好\nb";
    let index = LineIndex::new(text);
    let pos = index.offset_to_position(text, 5);
    assert_eq!(pos, Position { line: 1, character: 1 });
    let off = index.position_to_offset(text, Position { line: 1, character: 2 });
    assert_eq!(off, 8);
}

#[test]
fn lsp_hover_and_builtins() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn main() {\n    let ad = chan_new();\n}"}}}"#;
    server.handle_message(open);

    let hover_req = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":1,"character":15}}}"#;
    let resps = server.handle_message(hover_req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let hover_val = val.get("result").and_then(|r| r.get("contents")).and_then(|c| c.get("value")).and_then(|v| v.as_str()).unwrap();
    assert!(hover_val.contains("chan_new"));
}

#[test]
fn lsp_definition_jumping() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn compute() -> i64 { return 42; }\nfn main() {\n    print(compute());\n}"}}}"#;
    server.handle_message(open);

    let def_req = r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":2,"character":12}}}"#;
    let resps = server.handle_message(def_req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let res = val.get("result").unwrap();
    let uri = res.get("uri").and_then(|u| u.as_str()).unwrap();
    assert_eq!(uri, "file:///test.ae");
    let line = res.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_i64()).unwrap();
    assert_eq!(line, 0);
}

#[test]
fn lsp_completion_trigger_and_builtins() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"struct Point { x: i64, y: i64 }\nfn main() {\n    let p = Point { x: 1, y: 2 };\n    p.\n}"}}}"#;
    server.handle_message(open);

    let comp_req = r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":3,"character":6}}}"#;
    let resps = server.handle_message(comp_req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let items = val.get("result").and_then(|r| r.as_array()).unwrap();
    let labels: Vec<&str> = items.iter().filter_map(|i| i.get("label").and_then(|l| l.as_str())).collect();
    assert!(labels.contains(&"x") || labels.contains(&"y"));
}

#[test]
fn lsp_rename_symbol_across_file() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn main() {\n    let counter = 10;\n    print(counter);\n}"}}}"#;
    server.handle_message(open);

    let rename_req = r#"{"jsonrpc":"2.0","id":5,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":1,"character":10},"newName":"total"}}"#;
    let resps = server.handle_message(rename_req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let changes = val.get("result").and_then(|r| r.get("changes")).unwrap();
    let edits = changes.get("file:///test.ae").and_then(|e| e.as_array()).unwrap();
    assert_eq!(edits.len(), 2);
}

#[test]
fn lsp_cancel_stale_request() {
    let mut server = LspServer::new();
    let cancel = r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":99}}"#;
    server.handle_message(cancel);

    let req = r#"{"jsonrpc":"2.0","id":99,"method":"initialize","params":{}}"#;
    let resps = server.handle_message(req);
    assert_eq!(resps.len(), 1);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let err_code = val.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_i64()).unwrap();
    assert_eq!(err_code, -32800);
}


#[test]
fn lsp_positions_use_utf16_and_clamp_before_crlf() {
    let text = "a😀b\r\nnext";
    let index = LineIndex::new(text);
    for (offset, character) in [(0, 0), (1, 1), (5, 3), (6, 4)] {
        let position = Position { line: 0, character };
        assert_eq!(index.offset_to_position(text, offset), position);
        assert_eq!(index.position_to_offset(text, position), offset);
    }
    assert_eq!(index.position_to_offset(text, Position { line: 0, character: 2 }), 1);
    assert_eq!(index.position_to_offset(text, Position { line: 0, character: 99 }), 6);
    assert_eq!(index.offset_to_position(text, 7), Position { line: 0, character: 4 });
}

#[test]
fn lsp_publishes_current_diagnostics_and_clears_them_on_close() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":3,"text":"fn main() { print(missing); }"}}}"#;
    let diagnostics = JsonValue::parse(&server.handle_message(open)[0]).unwrap();
    let params = diagnostics.get("params").unwrap();
    assert_eq!(params.get("version").and_then(JsonValue::as_i64), Some(3));
    assert!(!params.get("diagnostics").unwrap().as_array().unwrap().is_empty());

    let stale = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///test.ae","version":2},"contentChanges":[{"text":"fn main() {}"}]}}"#;
    assert!(server.handle_message(stale).is_empty());

    let change = stale.replace("\"version\":2", "\"version\":4");
    let diagnostics = JsonValue::parse(&server.handle_message(&change)[0]).unwrap();
    let params = diagnostics.get("params").unwrap();
    assert_eq!(params.get("version").and_then(JsonValue::as_i64), Some(4));
    assert!(params.get("diagnostics").unwrap().as_array().unwrap().is_empty());

    let close = r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///test.ae"}}}"#;
    let diagnostics = JsonValue::parse(&server.handle_message(close)[0]).unwrap();
    let params = diagnostics.get("params").unwrap();
    assert_eq!(params.get("uri").and_then(JsonValue::as_str), Some("file:///test.ae"));
    assert!(params.get("diagnostics").unwrap().as_array().unwrap().is_empty());
}


#[test]
fn lsp_preserves_compiler_code_and_full_span_after_a_surrogate_pair() {
    let text = "fn main() { print(\"😀\"); print(missing); }";
    let start = text.find("missing").unwrap();
    let diagnostics = LspServer::new().check_text(text);
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.code.as_deref(), Some("E0101"));
    assert_eq!(diagnostic.range.start, Position {
        line: 0,
        character: text[..start].encode_utf16().count() as u32,
    });
    assert_eq!(diagnostic.range.end.character, diagnostic.range.start.character + 7);
}

#[test]
fn lsp_hover_renders_signature_from_the_def_table() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"struct Point { x: i64, y: i64 }\nfn area(p: &Point) -> i64 { return p.x; }\nfn main() {\n    let pt = Point { x: 1, y: 2 };\n    let s = area(&pt);\n}"}}}"#;
    server.handle_message(open);

    // The cursor sits on `area` inside the call; the def table names the
    // function and renders its parameter and return spellings.
    let hover_req = r#"{"jsonrpc":"2.0","id":6,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":4,"character":13}}}"#;
    let resps = server.handle_message(hover_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let hover = val.get("result").and_then(|r| r.get("contents")).and_then(|c| c.get("value")).and_then(|v| v.as_str()).unwrap();
    assert!(hover.contains("fn area(p: &Point) -> i64"), "hover was: {hover}");
}

#[test]
fn lsp_hover_shows_binding_type_from_the_def_table() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn main() {\n    let count = 7;\n    print(count);\n}"}}}"#;
    server.handle_message(open);

    // The cursor sits on `count` in `print(count)`; the binding's inferred
    // shape is i64.
    let hover_req = r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":2,"character":11}}}"#;
    let resps = server.handle_message(hover_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let hover = val.get("result").and_then(|r| r.get("contents")).and_then(|c| c.get("value")).and_then(|v| v.as_str()).unwrap();
    assert!(hover.contains("i64"), "hover was: {hover}");
}

#[test]
fn lsp_definition_lands_on_the_struct_name_range() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"struct Point { x: i64, y: i64 }\nfn main() {\n    let p = Point { x: 1, y: 2 };\n}"}}}"#;
    server.handle_message(open);

    let def_req = r#"{"jsonrpc":"2.0","id":8,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":2,"character":16}}}"#;
    let resps = server.handle_message(def_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let start = val.get("result").and_then(|r| r.get("range")).and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    let character = start.get("character").and_then(|c| c.as_i64()).unwrap();
    // The def's range is the name itself, not the whole declaration.
    assert_eq!(character, "struct ".encode_utf16().count() as i64);
}

#[test]
fn lsp_rename_is_scope_exact_via_the_def_table() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"fn a() -> i64 { let counter = 1; print(counter); return 0; }\nfn b() -> i64 { let counter = 2; print(counter); return 0; }\nfn main() {}"}}}"#;
    server.handle_message(open);

    // The cursor sits on fn a's `counter` declaration: only fn a's def and
    // its read move. The text fallback would have renamed all four tokens.
    let rename_req = r#"{"jsonrpc":"2.0","id":9,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":0,"character":20},"newName":"total"}}"#;
    let resps = server.handle_message(rename_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let edits = val.get("result").and_then(|r| r.get("changes")).and_then(|c| c.get("file:///test.ae")).and_then(|e| e.as_array()).unwrap();
    assert_eq!(edits.len(), 2, "scope-exact rename moved {edits:?}");
    let lines: Vec<i64> = edits.iter().filter_map(|e| e.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_i64())).collect();
    assert!(lines.iter().all(|line| *line == 0), "edits leaked into fn b: {lines:?}");
}

#[test]
fn lsp_completion_includes_file_defs_from_the_def_table() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"struct Point { x: i64, y: i64 }\nfn area(p: &Point) -> i64 { return p.x; }\nfn main() {\n    let width = 3;\n    \n}"}}}"#;
    server.handle_message(open);

    // Completion on the empty line inside main: the file's own defs join
    // the candidates with their declared shapes.
    let comp_req = r#"{"jsonrpc":"2.0","id":10,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///test.ae"},"position":{"line":4,"character":4}}}"#;
    let resps = server.handle_message(comp_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let items = val.get("result").and_then(|r| r.as_array()).unwrap();
    let entries: Vec<(String, String)> = items.iter().filter_map(|i| {
        let label = i.get("label").and_then(|l| l.as_str())?.to_owned();
        let detail = i.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_owned();
        Some((label, detail))
    }).collect();
    assert!(entries.iter().any(|(label, detail)| label == "area" && detail.contains("fn area(p: &Point) -> i64")), "entries: {entries:?}");
    assert!(entries.iter().any(|(label, detail)| label == "Point" && detail.contains("struct")), "entries: {entries:?}");
    assert!(entries.iter().any(|(label, detail)| label == "width" && detail.contains("i64")), "entries: {entries:?}");

    let source = concat!(
        "enum Choice { Some(i64), None }\n",
        "fn scoped(parameter: i64) -> i64 {\n",
        "    if true { let sibling = 1; print(sibling); }\n",
        "    else { let sibling = false; print(sibling); }\n",
        "    for index in 0..2 { print(index); }\n",
        "    let task = |argument: i64| -> i64 { return argument + parameter; };\n",
        "    let choice = Choice::Some(1);\n",
        "    match choice { Choice::Some(payload) => { print(payload); } Choice::None => {} }\n",
        "    return task(parameter);\n",
        "}\n",
        "fn shadow(outer_value: i64) -> i64 {\n",
        "    let inner = |outer_value: bool| -> bool { return outer_value; };\n",
        "    return outer_value;\n",
        "}",
    );
    let open = format!(r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///scope.ae","version":1,"text":{}}}}}}}"#,
        JsonValue::String(source.to_owned()).serialize());
    server.handle_message(&open);
    let index = LineIndex::new(source);
    let complete_at = |needle: &str| {
        server.completion("file:///scope.ae", index.offset_to_position(source, source.find(needle).unwrap()))
    };
    let local_names = |items: &[(String, u32, Option<String>)]| {
        items.iter().filter(|(_, kind, _)| *kind == 6).map(|(name, _, _)| name.clone()).collect::<Vec<_>>()
    };
    assert_eq!(local_names(&complete_at("false; print")), ["parameter"]);
    let sibling_items = complete_at("print(sibling); }\n    for");
    assert!(sibling_items.iter().any(|(name, _, detail)| {
        name == "sibling" && detail.as_deref() == Some("sibling: bool")
    }), "sibling block entries: {sibling_items:?}");
    assert_eq!(local_names(&complete_at("print(index)")), ["parameter", "index"]);
    assert_eq!(local_names(&complete_at("return argument")), ["parameter", "argument"]);
    assert_eq!(local_names(&complete_at("print(payload)")), ["parameter", "task", "choice", "payload"]);
    assert_eq!(local_names(&complete_at("return task")), ["parameter", "task", "choice"]);

    for (needle, expected) in [
        ("outer_value; };", "outer_value: bool"),
        ("outer_value;\n}", "outer_value: i64"),
    ] {
        let items = complete_at(needle);
        let matching: Vec<_> = items.iter().filter(|(name, _, _)| name == "outer_value").collect();
        assert_eq!(matching.len(), 1, "visible shadow: {items:?}");
        assert_eq!(matching[0].2.as_deref(), Some(expected));
    }
    let outer_return = index.offset_to_position(source, source.find("outer_value;\n}").unwrap());
    let inner_return = index.offset_to_position(source, source.find("outer_value; };").unwrap());
    let outer = server.definition("file:///scope.ae", outer_return).unwrap();
    let inner = server.definition("file:///scope.ae", inner_return).unwrap();
    assert!(outer.range.start.line < inner.range.start.line);
}

#[test]
fn lsp_definition_jumps_across_imported_files() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("ae-lsp-import-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let utils_source = "pub fn helper(x: i64) -> i64 { return x + 1; }";
    let main_source = "use \"utils.ae\";\nfn main() {\n    let value = helper(1);\n    print(value);\n}";
    std::fs::write(dir.join("utils.ae"), utils_source).unwrap();
    let main_path = dir.join(format!("main-{}.ae", NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&main_path, main_source).unwrap();

    let mut server = LspServer::new();
    let uri = format!("file:///{}", main_path.to_string_lossy().replace('\\', "/"));
    let open = format!(r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","version":1,"text":{}}}}}}}"#, JsonValue::String(main_source.to_owned()).serialize());
    server.handle_message(open.as_str());

    // The cursor sits on `helper` inside the call: the def table resolves
    // it to the imported pub function, whose definition site is the
    // import's own file.
    let def_req = format!(r#"{{"jsonrpc":"2.0","id":11,"method":"textDocument/definition","params":{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":2,"character":16}}}}}}"#);
    let resps = server.handle_message(def_req.as_str());
    let val = JsonValue::parse(&resps[0]).unwrap();
    let result = val.get("result").unwrap();
    let def_uri = result.get("uri").and_then(|u| u.as_str()).unwrap();
    assert!(def_uri.ends_with("utils.ae"), "definition uri was: {def_uri}");
    let start = result.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lsp_document_symbols_from_the_def_table() {
    let mut server = LspServer::new();
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.ae","version":1,"text":"struct Point { x: i64, y: i64 }\nfn area(p: &Point) -> i64 { return p.x; }\nfn main() { let pt = Point { x: 1, y: 2 }; let s = area(&pt); }"}}}"#;
    server.handle_message(open);

    let sym_req = r#"{"jsonrpc":"2.0","id":12,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///test.ae"}}}"#;
    let resps = server.handle_message(sym_req);
    let val = JsonValue::parse(&resps[0]).unwrap();
    let items = val.get("result").and_then(|r| r.as_array()).unwrap();
    let names: Vec<&str> = items.iter().filter_map(|i| i.get("name").and_then(|n| n.as_str())).collect();
    assert!(names.contains(&"Point"), "symbols: {names:?}");
    assert!(names.contains(&"area"), "symbols: {names:?}");
    assert!(names.contains(&"main"), "symbols: {names:?}");
    let area = items.iter().find(|i| i.get("name").and_then(|n| n.as_str()) == Some("area")).unwrap();
    let kind = area.get("kind").and_then(|k| k.as_i64()).unwrap();
    assert_eq!(kind, 12, "function symbol kind");
    let line = area.get("location").and_then(|l| l.get("range")).and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_i64()).unwrap();
    assert_eq!(line, 1);
}
