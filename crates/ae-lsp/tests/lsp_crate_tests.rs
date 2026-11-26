use std::collections::BTreeMap;

use ae_lsp::{
    CancelToken, JsonErrorKind, JsonValue, LineIndex, LspSession, Position, Range, SessionError,
    TextDocumentContentChangeEvent,
};
use ae_query::{QueryKey, QueryKind};

#[test]
fn json_surrogate_pair_decoding_and_roundtrip() {
    let parsed = JsonValue::parse(r#"{"emoji":"\uD83D\uDE00","text":"a\n\t\"b"}"#)
        .expect("surrogate pair should parse");
    assert_eq!(parsed.get("emoji").and_then(JsonValue::as_str), Some("😀"));
    assert_eq!(
        parsed.get("text").and_then(JsonValue::as_str),
        Some("a\n\t\"b")
    );

    // Supplementary CJK character U+20000 -> \uD840\uDC00
    let cjk = JsonValue::parse(r#""\uD840\uDC00""#).expect("supplementary CJK surrogate pair");
    assert_eq!(cjk.as_str(), Some("\u{20000}"));

    // Round-trip serialization and re-parse.
    let serialized = parsed.serialize();
    let reparsed = JsonValue::parse(&serialized).expect("re-parse serialized JSON");
    assert_eq!(parsed, reparsed);
}

#[test]
fn json_rejects_lone_surrogates_and_malformed_escapes() {
    // Lone high surrogate at end of string
    let err = JsonValue::parse(r#""\uD83D""#).expect_err("lone high surrogate must fail");
    assert_eq!(err.kind, JsonErrorKind::LoneSurrogate);

    // High surrogate followed by non-surrogate \u0041
    let err = JsonValue::parse(r#""\uD83D\u0041""#).expect_err("high + BMP must fail");
    assert_eq!(err.kind, JsonErrorKind::LoneSurrogate);

    // High surrogate followed by another high surrogate
    let err = JsonValue::parse(r#""\uD83D\uD83D""#).expect_err("high + high must fail");
    assert_eq!(err.kind, JsonErrorKind::LoneSurrogate);

    // Lone low surrogate
    let err = JsonValue::parse(r#""\uDE00""#).expect_err("lone low surrogate must fail");
    assert_eq!(err.kind, JsonErrorKind::LoneSurrogate);

    // Truncated \u escape
    let err = JsonValue::parse(r#""\u12""#).expect_err("short unicode escape must fail");
    assert_eq!(err.kind, JsonErrorKind::InvalidEscape);

    // Non-hex \u escape
    let err = JsonValue::parse(r#""\u12GZ""#).expect_err("non-hex unicode escape must fail");
    assert_eq!(err.kind, JsonErrorKind::InvalidEscape);

    // Unknown escape
    let err = JsonValue::parse(r#""\q""#).expect_err("unknown escape must fail");
    assert_eq!(err.kind, JsonErrorKind::InvalidEscape);

    // Unescaped control char
    let err = JsonValue::parse("\"a\nb\"").expect_err("unescaped newline must fail");
    assert_eq!(err.kind, JsonErrorKind::UnescapedControlChar);

    // Trailing characters
    let err = JsonValue::parse("123 456").expect_err("trailing token must fail");
    assert_eq!(err.kind, JsonErrorKind::TrailingCharacters);

    // Leading zero number
    let err = JsonValue::parse("01").expect_err("leading zero must fail");
    assert_eq!(err.kind, JsonErrorKind::InvalidNumber);
}

#[test]
fn json_object_and_array_serialization_roundtrip() {
    let mut obj = BTreeMap::new();
    obj.insert("null".to_owned(), JsonValue::Null);
    obj.insert("bool".to_owned(), JsonValue::Bool(true));
    obj.insert("int".to_owned(), JsonValue::Number(42.0));
    obj.insert(
        "arr".to_owned(),
        JsonValue::Array(vec![
            JsonValue::String("中文😀".to_owned()),
            JsonValue::Bool(false),
        ]),
    );
    let value = JsonValue::Object(obj);
    let json = value.serialize();
    let reparsed = JsonValue::parse(&json).expect("round-trip object");
    assert_eq!(value, reparsed);
}

#[test]
fn position_utf16_utf8_conversions_across_emoji_tabs_cjk_and_crlf() {
    // Line 0: "\t中😀a\r\n"
    //   '\t'  -> byte 0..1  (UTF-16 col 0..1)
    //   '中'  -> byte 1..4  (UTF-16 col 1..2)
    //   '😀'  -> byte 4..8  (UTF-16 col 2..4, 2 UTF-16 units)
    //   'a'   -> byte 8..9  (UTF-16 col 4..5)
    //   "\r\n"-> byte 9..11
    // Line 1: "\u{20000}b\n"
    //   '\u{20000}' -> byte 11..15 (UTF-16 col 0..2, 2 UTF-16 units)
    //   'b'         -> byte 15..16 (UTF-16 col 2..3)
    //   '\n'        -> byte 16..17
    // Line 2: "end" (byte 17..20)
    let text = "\t中😀a\r\n\u{20000}b\nend";
    let index = LineIndex::new(text);

    assert_eq!(index.line_count(), 3);

    // Line 0 exact boundaries
    assert_eq!(index.offset_to_position(text, 0), Position::new(0, 0));
    assert_eq!(index.offset_to_position(text, 1), Position::new(0, 1));
    assert_eq!(index.offset_to_position(text, 4), Position::new(0, 2));
    assert_eq!(index.offset_to_position(text, 8), Position::new(0, 4));
    assert_eq!(index.offset_to_position(text, 9), Position::new(0, 5));
    // Offset on '\n' in "\r\n" clamps to line end before "\r\n"
    assert_eq!(index.offset_to_position(text, 10), Position::new(0, 5));

    // Mid-UTF-8 byte inside '😀' (byte 6) floors to start of '😀' (col 2)
    assert_eq!(index.offset_to_position(text, 6), Position::new(0, 2));

    // Position -> offset
    assert_eq!(index.position_to_offset(text, Position::new(0, 0)), 0);
    assert_eq!(index.position_to_offset(text, Position::new(0, 1)), 1);
    assert_eq!(index.position_to_offset(text, Position::new(0, 2)), 4);
    // Mid-surrogate-pair UTF-16 column (col 3) floors to start of '😀' (byte 4)
    assert_eq!(index.position_to_offset(text, Position::new(0, 3)), 4);
    assert_eq!(index.position_to_offset(text, Position::new(0, 4)), 8);
    assert_eq!(index.position_to_offset(text, Position::new(0, 5)), 9);
    // Out-of-bounds character on line 0 clamps to line_end (9, before \r\n)
    assert_eq!(index.position_to_offset(text, Position::new(0, 99)), 9);

    // Line 1 supplementary CJK
    assert_eq!(index.offset_to_position(text, 11), Position::new(1, 0));
    assert_eq!(index.offset_to_position(text, 15), Position::new(1, 2));
    assert_eq!(index.offset_to_position(text, 16), Position::new(1, 3));
    assert_eq!(index.position_to_offset(text, Position::new(1, 0)), 11);
    assert_eq!(index.position_to_offset(text, Position::new(1, 1)), 11);
    assert_eq!(index.position_to_offset(text, Position::new(1, 2)), 15);
    assert_eq!(index.position_to_offset(text, Position::new(1, 3)), 16);

    // Out-of-bounds line and offset clamping
    assert_eq!(index.position_to_offset(text, Position::new(99, 0)), text.len());
    assert_eq!(index.offset_to_position(text, 9999), Position::new(2, 3));
}

#[test]
fn incremental_did_change_matches_fresh_did_open_buffer_and_diagnostics() {
    let initial = "fn main() {\n    // 😀\n    let x = 1;\n}\nfn broken(\n";
    let expected_final = "fn main() {\n    // 😀中文\n    let x = 42;\n}\nfn broken(\n";

    let mut incremental_session = LspSession::new();
    incremental_session.did_open("file:///test.ae", 1, initial);

    // Apply two sequential incremental edits in one did_change batch:
    // 1. Append "中文" after "😀" on line 1 (UTF-16 col 9: "    // " is 7 + "😀" is 2 = 9).
    // 2. Replace "1" on line 2 with "42".
    let changes = vec![
        TextDocumentContentChangeEvent::incremental(
            Range::new(Position::new(1, 9), Position::new(1, 9)),
            "中文",
        ),
        TextDocumentContentChangeEvent::incremental(
            Range::new(Position::new(2, 12), Position::new(2, 13)),
            "42",
        ),
    ];
    incremental_session
        .did_change("file:///test.ae", 2, &changes)
        .expect("incremental did_change succeeds");

    assert_eq!(
        incremental_session.document_text("file:///test.ae"),
        Some(expected_final)
    );
    assert_eq!(incremental_session.document_version("file:///test.ae"), Some(2));

    let mut fresh_session = LspSession::new();
    fresh_session.did_open("file:///test.ae", 2, expected_final);

    let cancel = CancelToken::new();
    let inc_diags = incremental_session
        .diagnostics("file:///test.ae", &cancel)
        .expect("incremental diagnostics");
    let fresh_diags = fresh_session
        .diagnostics("file:///test.ae", &cancel)
        .expect("fresh diagnostics");
    assert_eq!(inc_diags, fresh_diags);
    assert!(!inc_diags.is_empty(), "invalid syntax should produce diagnostics");

    // Full-document sync via did_change
    incremental_session
        .did_change(
            "file:///test.ae",
            3,
            &[TextDocumentContentChangeEvent::full("fn main() {}\n")],
        )
        .expect("full did_change succeeds");
    let clean_diags = incremental_session
        .diagnostics("file:///test.ae", &cancel)
        .expect("clean diagnostics");
    assert!(clean_diags.is_empty());
}

#[test]
fn early_cutoff_across_private_body_edits_via_query_db() {
    let mut session = LspSession::new();
    let lib_uri = "file:///lib.ae";
    let app_uri = "file:///app.ae";

    let lib_v1 = "pub fn helper() -> i64 {\n    return 1;\n}\n";
    let app_src = "use lib;\npub fn main() -> i64 {\n    return 0;\n}\n";

    let _lib_id = session.did_open(lib_uri, 1, lib_v1);
    let app_id = session.did_open(app_uri, 1, app_src);
    session
        .register_import(app_uri, lib_uri)
        .expect("register import edge");

    let cancel = CancelToken::new();
    let check_1 = session
        .check_with_cancel(app_uri, &cancel)
        .expect("initial check succeeds");
    let app_check_key = QueryKey {
        kind: QueryKind::CheckFile,
        file: app_id,
    };
    assert_eq!(session.db().recompute_count(app_check_key), 1);

    // Edit only the private body of helper() in lib.ae (1 -> 999).
    session
        .did_change(
            lib_uri,
            2,
            &[TextDocumentContentChangeEvent::incremental(
                Range::new(Position::new(1, 11), Position::new(1, 12)),
                "999",
            )],
        )
        .expect("edit private body");

    let check_2 = session
        .check_with_cancel(app_uri, &cancel)
        .expect("post-edit check succeeds");
    assert_eq!(check_1.fingerprint, check_2.fingerprint);
    // Early cutoff: app.ae's CheckFile query must NOT recompute!
    assert_eq!(session.db().recompute_count(app_check_key), 1);
}

#[test]
fn cancel_token_aborts_stale_queries_and_close_removes_document() {
    let mut session = LspSession::new();
    let uri = "file:///cancel.ae";
    session.did_open(uri, 1, "fn main() {}\n");

    let cancel = CancelToken::new();
    cancel.cancel();

    assert_eq!(
        session.diagnostics(uri, &cancel),
        Err(SessionError::Cancelled)
    );
    assert_eq!(
        session.check_with_cancel(uri, &cancel),
        Err(SessionError::Cancelled)
    );

    assert!(session.did_close(uri));
    assert!(!session.is_open(uri));
    assert!(!session.did_close(uri));

    let active = CancelToken::new();
    assert_eq!(
        session.diagnostics(uri, &active),
        Err(SessionError::UnknownDocument(uri.to_owned()))
    );
}
