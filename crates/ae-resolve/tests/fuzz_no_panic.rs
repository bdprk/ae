use ae_resolve::{dump, parse_module, resolve, resolve::resolve_with_imports};

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
    "fn main() { let x = 1; print(x); }",
    "use lib;\npub struct Point { x: i64, y: i64 }\npub fn make(x: i64) -> Point { return Point { x: x, y: 0 }; }",
    "enum Opt[T] { Some(T), None }\nfn unwrap_or(o: Opt[i64]) -> i64 { match o { Some(v) => v, None => 0 } }",
    "impl Point { pub fn shift(mut self, dx: i64) { self.x = self.x + dx; } }",
    "fn loops() { for i in 0..10 { if i == 5 { break; } } let f = |x: i64| -> i64 { return x + 1; }; }",
    "/// safety\nunsafe fn raw(p: *mut i64) -> i64 { return *p; }",
    "fn unicode() { let s = \"中😀\"; let mut a = 1; a = a + 1; }",
    "fn broken( { let x = ; }",
];

const FRAGMENTS: &[&str] = &[
    "fn ", "pub ", "struct ", "enum ", "impl ", "trait ", "use ", "let ", "mut ", "match ",
    "for ", "in ", "if ", "else ", "while ", "return ", "unsafe ", "{", "}", "(", ")", "[", "]",
    "=>", "->", "::", "..", ";", ":", ",", ".", "self", "Self", "_", "中", "😀", "\"", "\n",
];

fn mutate_utf8(rng: &mut Rng, base: &str) -> String {
    let mut chars: Vec<char> = base.chars().collect();
    let ops = 1 + rng.usize(5);
    for _ in 0..ops {
        match rng.usize(3) {
            0 if !chars.is_empty() => {
                let len = rng.usize(chars.len() + 1);
                chars.truncate(len);
            }
            1 if !chars.is_empty() => {
                let start = rng.usize(chars.len());
                let end = start + rng.usize(chars.len() - start + 1);
                chars.drain(start..end);
            }
            _ => {
                let frag = FRAGMENTS[rng.usize(FRAGMENTS.len())];
                let pos = rng.usize(chars.len() + 1);
                let inserted: Vec<char> = frag.chars().collect();
                chars.splice(pos..pos, inserted);
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn deterministic_fuzz_resolver_zero_panics() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let import_mod = parse_module(
        "lib.ae",
        "pub struct Exported { v: i64 }\npub fn helper(x: i64) -> i64 { return x; }",
    )
    .expect("valid import seed");
    let imports = [("lib".to_owned(), import_mod)];

    for &seed in SEEDS {
        if let Ok(module) = parse_module("fuzz.ae", seed) {
            let res = resolve(&module);
            let _ = dump(&module, &res);
            let res_imp = resolve_with_imports(&module, &imports);
            let _ = dump(&module, &res_imp);
        }
    }

    for case_idx in 0..2000 {
        let base = SEEDS[case_idx % SEEDS.len()];
        let mutated = mutate_utf8(&mut rng, base);
        if let Ok(module) = parse_module("fuzz.ae", &mutated) {
            let res = resolve(&module);
            let _ = dump(&module, &res);
            let res_imp = resolve_with_imports(&module, &imports);
            let _ = dump(&module, &res_imp);
        }
    }
}
