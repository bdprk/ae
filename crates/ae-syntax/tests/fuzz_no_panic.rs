use ae_syntax::{
    declaration::{parse_declarations, recover_declarations},
    diagnostic_range,
    expression::parse_operator_expression,
    fmt, has_doc_comment_above, lex, lex_diagnostic, lex_recovered, lex_with_diagnostics,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() as usize) % bound
        }
    }
}

const SEEDS: &[&str] = &[
    "",
    "fn main() { print(1); }",
    "/// doc comment\nunsafe fn danger(x: *const i64) -> i64 { return *x; }",
    "struct Pair[T, U] { first: T, second: U }\nenum Shape { Circle(f64), Rect(f64, f64), Empty }",
    "impl Shape { pub fn area(self) -> f64 { match self { Circle(r) => r * r, _ => 0.0 } } }",
    "fn unicode() { let s = \"中😀\\n\\x\"; let 你好 = 1; // comment\n\tlet x = 1..10; }",
    "fn closure() { let f = |a: i64, mut b: string| -> i64 { return a; }; }",
    "use std::io;\n#[no_mangle]\nextern \"C\" fn c_api(p: *mut u8) -> i32;",
    "\"unterminated \\",
    "0x123_abc_xyz 0b102 1e9999999999999999999",
    "(((((((((([[[[[[[[[[{{{{{{{{{{",
    "fn bad(a: i64, b: ) { if true { let x = 1 + * / 2; } else { for i in 0.. { break; } } }",
];

const FRAGMENTS: &[&str] = &[
    "fn ", "let ", "mut ", "if ", "else ", "match ", "while ", "for ", "in ", "return ", "unsafe ",
    "struct ", "enum ", "trait ", "impl ", "use ", "pub ", "\"", "\\", "\n", "\0", "//", "///",
    "{", "}", "(", ")", "[", "]", "<", ">", "->", "=>", "::", "..", "?", ";", ":", ",", ".",
    "中", "😀", "\t", "\r\n", "a\u{0301}", "0x", "0b", "1.5e-", "!", "*", "&", "|", "=", "+",
];

fn mutate_utf8(rng: &mut Rng, base: &str) -> String {
    let mut chars: Vec<char> = base.chars().collect();
    let ops = 1 + rng.usize(6);
    for _ in 0..ops {
        match rng.usize(4) {
            0 if !chars.is_empty() => {
                // Truncate at a random char boundary
                let len = rng.usize(chars.len() + 1);
                chars.truncate(len);
            }
            1 if !chars.is_empty() => {
                // Delete a random slice
                let start = rng.usize(chars.len());
                let end = start + rng.usize(chars.len() - start + 1);
                chars.drain(start..end);
            }
            2 => {
                // Insert a random grammar/UTF-8 fragment
                let frag = FRAGMENTS[rng.usize(FRAGMENTS.len())];
                let pos = rng.usize(chars.len() + 1);
                let inserted: Vec<char> = frag.chars().collect();
                chars.splice(pos..pos, inserted);
            }
            _ => {
                // Duplicate a slice (stress nesting / limits)
                if !chars.is_empty() {
                    let start = rng.usize(chars.len());
                    let end = (start + 1 + rng.usize(8)).min(chars.len());
                    let slice: Vec<char> = chars[start..end].to_vec();
                    let pos = rng.usize(chars.len() + 1);
                    chars.splice(pos..pos, slice);
                }
            }
        }
    }
    chars.into_iter().collect()
}

fn exercise_syntax(source: &str, rng: &mut Rng) {
    let _ = lex(source);
    let _ = lex_diagnostic(source);
    let _ = lex_with_diagnostics(source);
    let _ = lex_recovered(source);
    let _ = parse_declarations("fuzz.ae", source);
    let _ = recover_declarations("fuzz.ae", source);
    let _ = parse_operator_expression("fuzz.ae", source);
    let _ = fmt::format_source(source);

    // Exercise arbitrary offsets (including non-UTF-8 boundaries and out-of-bounds)
    let len = source.len();
    let off1 = rng.usize(len + 8);
    let off2 = rng.usize(len + 8);
    let _ = has_doc_comment_above(source, off1);
    let _ = diagnostic_range("fuzz.ae", source, off1..off2, "fuzz");
    let _ = diagnostic_range("fuzz.ae", source, off2..off1, "fuzz");
}

#[test]
fn deterministic_fuzz_lexer_parser_formatter_zero_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for &seed in SEEDS {
        exercise_syntax(seed, &mut rng);
        // Test every UTF-8 prefix truncation of each seed
        for (idx, _) in seed.char_indices() {
            exercise_syntax(&seed[..idx], &mut rng);
        }
    }
    for case_idx in 0..3000 {
        let base = SEEDS[case_idx % SEEDS.len()];
        let mutated = mutate_utf8(&mut rng, base);
        exercise_syntax(&mutated, &mut rng);
    }
}
