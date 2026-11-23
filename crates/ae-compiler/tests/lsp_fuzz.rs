use std::io::Cursor;

use ae_compiler::lsp::{read_lsp_message, run_lsp_loop, LspServer, Position};

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

const FRAME_SEEDS: &[&[u8]] = &[
    b"",
    b"\r\n\r\n",
    b"Content-Length: 0\r\n\r\n",
    b"Content-Length: -1\r\n\r\n{}",
    b"Content-Length: abc\r\n\r\n{}",
    b"Content-Length: 999999999999999999\r\n\r\n{}",
    b"Content-Length: 100\r\n\r\n{\"jsonrpc\":\"2.0\"}",
    b"Content-Length: 4\r\n\r\n\xff\xfe\xfd\xfc",
    b"Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
    b"X-Header: foo\r\n\r\n",
];

const JSON_SEEDS: &[&str] = &[
    "",
    "null",
    "[]",
    "{}",
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}",
    "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\",\"languageId\":\"ae\",\"version\":1,\"text\":\"fn main() { let x = 1; }\"}}}",
    "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\",\"version\":2},\"contentChanges\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":99,\"character\":999}},\"text\":\"中😀\\n\"}]}}",
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"textDocument/hover\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\"},\"position\":{\"line\":0,\"character\":5}}}",
    "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"textDocument/definition\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\"},\"position\":{\"line\":10,\"character\":100}}}",
    "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"textDocument/completion\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\"},\"position\":{\"line\":0,\"character\":0}}}",
    "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"textDocument/rename\",\"params\":{\"textDocument\":{\"uri\":\"file:///fuzz.ae\"},\"position\":{\"line\":0,\"character\":4},\"newName\":\"y\"}}",
    "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"shutdown\"}",
    "{\"broken\": \"\\uD800\\uZZZZ",
    "{\"a\":[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]}",
];

fn mutate_bytes(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    let ops = 1 + rng.usize(6);
    for _ in 0..ops {
        match rng.usize(4) {
            0 if !out.is_empty() => {
                let len = rng.usize(out.len() + 1);
                out.truncate(len);
            }
            1 if !out.is_empty() => {
                let idx = rng.usize(out.len());
                out[idx] = (rng.next() & 0xff) as u8;
            }
            2 => {
                let pos = rng.usize(out.len() + 1);
                out.insert(pos, (rng.next() & 0xff) as u8);
            }
            _ if !out.is_empty() => {
                let idx = rng.usize(out.len());
                out.remove(idx);
            }
            _ => {}
        }
    }
    out
}

fn mutate_str(rng: &mut Rng, base: &str) -> String {
    let frags = [
        "{", "}", "[", "]", ":", ",", "\"", "null", "true", "1e999", "\\uD83D\\uDE00", "\\uZZZZ",
        "textDocument", "uri", "position", "line", "character", "newName", "中", "😀", "\n",
    ];
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
                let frag = frags[rng.usize(frags.len())];
                let pos = rng.usize(chars.len() + 1);
                chars.splice(pos..pos, frag.chars());
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn deterministic_fuzz_lsp_frames_and_handlers_zero_panics() {
    let mut rng = Rng(0xA076_1D64_78BD_642F);

    for &seed in FRAME_SEEDS {
        let mut cursor = Cursor::new(seed);
        let _ = read_lsp_message(&mut cursor);
        let mut out = Vec::new();
        let _ = run_lsp_loop(Cursor::new(seed), &mut out);
    }

    for i in 0..2000 {
        let base = FRAME_SEEDS[i % FRAME_SEEDS.len()];
        let mutated = mutate_bytes(&mut rng, base);
        let mut cursor = Cursor::new(&mutated);
        let _ = read_lsp_message(&mut cursor);
        let mut out = Vec::new();
        let _ = run_lsp_loop(Cursor::new(&mutated), &mut out);
    }

    let mut server = LspServer::new();
    for &seed in JSON_SEEDS {
        let _ = server.handle_message(seed);
    }

    for i in 0..2000 {
        let base = JSON_SEEDS[i % JSON_SEEDS.len()];
        let mutated = mutate_str(&mut rng, base);
        let _ = server.handle_message(&mutated);
        if i % 10 == 0 {
            let line = rng.usize(20) as u32;
            let character = rng.usize(80) as u32;
            let _ = server.rename("file:///fuzz.ae", Position { line, character }, "renamed_ident");
        }
    }
}
