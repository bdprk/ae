//! Robustness: malformed input must produce a diagnostic, never a panic
//! (checklist §8 phase P0.4, Hard Rule 3's "structured" side).
//!
//! The corpus files are the seed material. Each case applies a deterministic
//! mutation — truncation, single-byte flip, delimiter insertion — and then
//! asks every publicly reachable front-end entry to process it. A panic
//! anywhere fails the test; the entries themselves decide accept vs reject.

#[cfg(test)]
mod tests {
    use crate::frontend::{check_source_diagnostic, reset_compilation_state};
    use crate::lsp::LspServer;

    /// The example sources, read at compile time so the test never depends
    /// on a working directory.
    const SEEDS: &[&str] = &[
        include_str!("../../../examples/hello.ae"),
        include_str!("../../../examples/vectors.ae"),
        include_str!("../../../examples/strings.ae"),
        include_str!("../../../examples/traits.ae"),
        include_str!("../../../examples/methods.ae"),
        include_str!("../../../examples/closures.ae"),
        include_str!("../../../examples/struct_borrow.ae"),
        include_str!("../../../examples/option_propagate.ae"),
        include_str!("../../../examples/pairs.ae"),
    ];

    /// Deterministic xorshift so a failure is reproducible from its index.
    fn next(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn mutate(seed: &str, state: &mut u64) -> String {
        let bytes = seed.as_bytes();
        let mut out = bytes.to_vec();
        match next(state) % 4 {
            // Truncate at a random byte (it may split a UTF-8 sequence).
            0 => {
                let at = (next(state) as usize) % (out.len() + 1);
                out.truncate(at);
            }
            // Flip one byte.
            1 if !out.is_empty() => {
                let at = (next(state) as usize) % out.len();
                out[at] ^= 0x01 << (next(state) % 8);
            }
            // Insert a delimiter pair at a random spot.
            2 => {
                let at = (next(state) as usize) % (out.len() + 1);
                let pair: &[u8] = match next(state) % 4 {
                    0 => b"{}",
                    1 => b"()",
                    2 => b"[]",
                    _ => b"\"\"",
                };
                let tail = out.split_off(at);
                out.extend_from_slice(pair);
                out.extend_from_slice(&tail);
            }
            // Delete a slice.
            _ if out.len() > 1 => {
                let at = (next(state) as usize) % out.len();
                let len = 1 + (next(state) as usize) % (out.len() - at);
                out.drain(at..at + len);
            }
            _ => {}
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn mutated_sources_never_panic_the_frontend() {
        let mut state = 0x5eed_1234_abcd_0001u64;
        let mut cases = 0usize;
        // The default sweep: 10,000 mutations over nine seeds. Each case
        // runs through the real entry (which hops to a 16 MB worker
        // thread), so the suite stays around eight seconds; the 50,000
        // audit gate is the #[ignore] twin below, run by its own CI step.
        for _ in 0..10_000 {
            let seed = SEEDS[(next(&mut state) as usize) % SEEDS.len()];
            let source = mutate(seed, &mut state);
            reset_compilation_state();
            let _ = ae_syntax::lex_diagnostic(&source);
            let _ = check_source_diagnostic(&source);
            cases += 1;
        }
        assert_eq!(cases, 10_000);
    }

    // One structured regression for the crash found during this audit: a
    // complete `main` behind a malformed generic body used to drive the
    // front end past a 2 MB thread's stack (the large-stack worker in
    // `check_source_diagnostic` answers with a diagnostic instead).
    #[test]
    fn malformed_generic_body_before_a_complete_main_returns_a_diagnostic() {
        let source = "trait Area { fn area(&self) -> i64; }\n\
            struct Rect { w: i64, h: i64 }\n\
            impl Area for Rect { fn area(&self) -> i64 { return self.w * self.h; } }\n\
            fn report[T: Area](shape: T) -> string { return \"area=\" + to_string(area(shape-); }\n\
            fn main() { print(report(Rect { w: 3, h: 4 })); }\n";
        assert!(check_source_diagnostic(source).is_err(), "malformed body must be rejected");
    }

    #[test]
    #[ignore = "the P0.4 audit gate: 50,000 mutations; run by the CI robustness step"]
    fn mutated_sources_never_panic_50k() {
        let mut state = 0xfeed_face_cafe_0002u64;
        for _ in 0..50_000 {
            let seed = SEEDS[(next(&mut state) as usize) % SEEDS.len()];
            let source = mutate(seed, &mut state);
            let _ = ae_syntax::lex_diagnostic(&source);
            let _ = check_source_diagnostic(&source);
        }
    }

    #[test]
    fn malformed_json_rpc_frames_never_panic_the_lsp() {
        let mut server = LspServer::new();
        let frames = [
            "",
            "{",
            "{}",
            "[]",
            "{\"jsonrpc\":null}",
            "{\"method\":\"textDocument/hover\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"textDocument/didOpen\",\"params\":{}}",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"textDocument/hover\",\"params\":{\"textDocument\":{\"uri\":\"file:///x.ae\"},\"position\":{\"line\":-1,\"character\":-1}}}",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"textDocument/semanticTokens/full\",\"params\":{\"textDocument\":{\"uri\":\"file:///nope.ae\"}}}",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"textDocument/completion\",\"params\":{\"textDocument\":{\"uri\":\"file:///x.ae\"},\"position\":{\"line\":4294967295,\"character\":4294967295}}}",
            "\u{0}\u{1}\u{2}",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///b.ae\",\"version\":1,\"text\":\"fn main() {\"}}}",
        ];
        for frame in frames {
            let _ = server.handle_message(frame);
        }
    }

}
