//! Shared test helpers (included with `mod common;`).
#![allow(dead_code, missing_docs)]

use crucible_term_core::Session;

/// xorshift64, as in the spike harness.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "fn", "let", "mut", "{", "}", "=>", "λ", "→",
    "✓", "漢字", "한글", "e\u{301}", "👍",
];

/// `lines` CRLF lines of mixed text (some longer than 200 columns, so they soft-wrap), with
/// SGR colour/attribute changes — the stand-in for the spike's corpus (a).
pub fn text_corpus(lines: usize) -> Vec<u8> {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut o = String::new();
    for _ in 0..lines {
        let max = if r.below(20) == 0 { 60 } else { 14 };
        let words = 1 + r.below(max);
        for _ in 0..words {
            match r.below(12) {
                0 => o.push_str(&format!(
                    "\x1b[38;2;{};{};{}m",
                    r.below(256),
                    r.below(256),
                    r.below(256)
                )),
                1 => o.push_str(&format!("\x1b[48;5;{}m", r.below(256))),
                2 => o.push_str(
                    [
                        "\x1b[1m",
                        "\x1b[3m",
                        "\x1b[2m",
                        "\x1b[4:3m",
                        "\x1b[7m",
                        "\x1b[9m",
                        "\x1b[31m",
                        "\x1b[94m",
                    ][r.below(8) as usize],
                ),
                3 => o.push_str("\x1b[0m"),
                _ => {}
            }
            o.push_str(WORDS[r.below(WORDS.len() as u64) as usize]);
            o.push(' ');
        }
        o.push_str("\x1b[0m\r\n");
    }
    o.into_bytes()
}

/// Escape-heavy edits on a 200x50 screen (the spike's corpus (b) shape, minus alt screen).
pub fn edit_corpus(seed: u64, target: usize) -> Vec<u8> {
    let mut r = Rng(seed);
    let mut o = String::new();
    while o.len() < target {
        match r.below(100) {
            0..=19 => o.push_str(&format!(
                "\x1b[38;2;{};{};{}m",
                r.below(256),
                r.below(256),
                r.below(256)
            )),
            20..=29 => o.push_str(&format!(
                "\x1b[{};48;5;{}m",
                [1, 2, 3, 4, 7, 9, 22, 23, 24, 27][r.below(10) as usize],
                r.below(256)
            )),
            30..=49 => o.push_str(&format!("\x1b[{};{}H", r.below(50) + 1, r.below(200) + 1)),
            50..=59 => {
                let ops = ["\x1b[K", "\x1b[1K", "\x1b[2K", "\x1b[J", "\x1b[1J"];
                o.push_str(ops[r.below(5) as usize]);
                o.push_str(&format!(
                    "\x1b[{}{}",
                    r.below(20) + 1,
                    ['X', '@', 'P', 'L', 'M'][r.below(5) as usize]
                ));
            }
            60..=64 => o.push_str("\x1b[0m"),
            _ => {
                for _ in 0..(1 + r.below(8)) {
                    o.push_str(WORDS[r.below(WORDS.len() as u64) as usize]);
                    o.push(' ');
                }
            }
        }
    }
    o.into_bytes()
}

pub fn fed(cols: usize, rows: usize, sb: usize, bytes: &[u8]) -> Session {
    let mut s = Session::new(cols, rows, sb);
    for ch in bytes.chunks(65536) {
        s.feed(ch);
    }
    s
}

/// Text of visible row `y`, trimmed.
pub fn line(s: &Session, y: usize) -> String {
    s.screen_row(y).text()
}
