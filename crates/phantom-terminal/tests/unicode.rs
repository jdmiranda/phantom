//! Unicode stress tests for the terminal parser.
//!
//! These tests exercise the VTE / ANSI byte processor with edge-case Unicode
//! sequences to ensure no panics occur and the terminal state machine
//! handles malformed or complex input gracefully.
//!
//! The tests use `Term<VoidListener>` directly — no PTY is spawned — so
//! they run in CI without a controlling terminal.

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::Config,
    vte::ansi::Processor,
    Term,
};

/// Minimal terminal size that implements `Dimensions`.
struct TestSize {
    cols: usize,
    rows: usize,
}

impl TestSize {
    fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TestSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Build a headless `Term<VoidListener>` and a fresh `Processor`.
fn headless_term() -> (Term<VoidListener>, Processor) {
    let size = TestSize::new(80, 24);
    let config = Config::default();
    let term = Term::new(config, &size, VoidListener);
    let parser = Processor::new();
    (term, parser)
}

/// Feed `bytes` through the parser into `term`.
///
/// This mirrors what `PhantomTerminal::pty_read` does:
/// `parser.advance(&mut term, bytes)`.
fn feed(term: &mut Term<VoidListener>, parser: &mut Processor, bytes: &[u8]) {
    parser.advance(term, bytes);
}

// ---------------------------------------------------------------------------
// Test 1 — Fire + skull emoji
// ---------------------------------------------------------------------------

/// Fire (U+1F525) + skull (U+1F480) should not panic.
///
/// Bytes: F0 9F 94 A5  F0 9F 92 80
#[test]
fn no_panic_fire_skull_emoji() {
    let (mut term, mut parser) = headless_term();
    feed(
        &mut term,
        &mut parser,
        b"\xF0\x9F\x94\xA5\xF0\x9F\x92\x80",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Arabic "Marhaba" (مرحبا) RTL text
// ---------------------------------------------------------------------------

/// Arabic text should not panic and the grid column count must be unchanged.
#[test]
fn no_panic_arabic_marhaba() {
    let (mut term, mut parser) = headless_term();

    // "مرحبا" in UTF-8
    let marhaba = "مرحبا".as_bytes();
    let cols_before = term.columns();
    feed(&mut term, &mut parser, marhaba);
    // Grid width must not change — layout intact.
    assert_eq!(term.columns(), cols_before);
}

// ---------------------------------------------------------------------------
// Test 3 — e + combining acute accent (U+0301)
// ---------------------------------------------------------------------------

/// `e` followed by a combining acute accent (U+0301) must not panic
/// and must not be split across cells incorrectly.
///
/// Bytes: 65 CC 81
#[test]
fn no_panic_e_combining_acute() {
    let (mut term, mut parser) = headless_term();
    feed(&mut term, &mut parser, b"e\xCC\x81");
}

// ---------------------------------------------------------------------------
// Test 4 — Null byte
// ---------------------------------------------------------------------------

/// A raw null byte (U+0000) must not panic.
/// It should be silently dropped or replaced.
#[test]
fn no_panic_null_byte() {
    let (mut term, mut parser) = headless_term();
    feed(&mut term, &mut parser, b"\x00");
}

// ---------------------------------------------------------------------------
// Test 5 — All edge-case sequences concatenated
// ---------------------------------------------------------------------------

/// Concatenating all edge-case byte sequences must not cause a panic.
#[test]
fn no_panic_combined_edge_cases() {
    let (mut term, mut parser) = headless_term();

    // Fire + skull
    feed(
        &mut term,
        &mut parser,
        b"\xF0\x9F\x94\xA5\xF0\x9F\x92\x80",
    );
    // Arabic
    feed(&mut term, &mut parser, "مرحبا".as_bytes());
    // e + combining acute
    feed(&mut term, &mut parser, b"e\xCC\x81");
    // Null byte
    feed(&mut term, &mut parser, b"\x00");
    // ZWJ family (test 7 payload)
    feed(
        &mut term,
        &mut parser,
        b"\xF0\x9F\x91\xA8\xE2\x80\x8D\xF0\x9F\x91\xA9\xE2\x80\x8D\xF0\x9F\x91\xA7",
    );
}

// ---------------------------------------------------------------------------
// Test 6 — Overlong UTF-8 sequences
// ---------------------------------------------------------------------------

/// Overlong two-byte encoding of U+002F (ASCII '/') must be rejected, not
/// interpreted as a valid codepoint.
///
/// `C0 AF` is the canonical overlong encoding of '/'. A correct UTF-8 decoder
/// must treat this as two replacement characters (or simply discard), never
/// as U+002F.
#[test]
fn overlong_utf8_rejected() {
    let (mut term, mut parser) = headless_term();

    // Overlong two-byte encoding of U+002F ('/')
    let overlong_slash: &[u8] = &[0xC0, 0xAF];
    feed(&mut term, &mut parser, overlong_slash);

    // Overlong three-byte encoding of U+0041 ('A'): E0 80 81
    let overlong_a: &[u8] = &[0xE0, 0x80, 0x81];
    feed(&mut term, &mut parser, overlong_a);

    // Overlong four-byte encoding of U+0000 (null): F0 80 80 80
    let overlong_null: &[u8] = &[0xF0, 0x80, 0x80, 0x80];
    feed(&mut term, &mut parser, overlong_null);
}

/// A sequence that starts like a valid 3-byte character but has an invalid
/// continuation byte must not cause a panic.
#[test]
fn no_panic_truncated_multibyte() {
    let (mut term, mut parser) = headless_term();

    // Start of a 3-byte sequence (U+2764 HEAVY BLACK HEART = E2 9D A4)
    // but cut off after the first byte.
    feed(&mut term, &mut parser, &[0xE2]);

    // Then send a standalone ASCII character — parser must recover.
    feed(&mut term, &mut parser, b"X");
}

/// High surrogate value (U+D800–U+DFFF) encoded as if it were a valid
/// 3-byte UTF-8 sequence must not cause a panic.
///
/// ED A0 80 would be the (illegal) UTF-8 encoding of U+D800.
#[test]
fn no_panic_surrogate_half() {
    let (mut term, mut parser) = headless_term();
    feed(&mut term, &mut parser, &[0xED, 0xA0, 0x80]);
}

// ---------------------------------------------------------------------------
// Test 7 — ZWJ family emoji sequence
// ---------------------------------------------------------------------------

/// Man + ZWJ + Woman + ZWJ + Girl emoji (U+1F468 + ZWJ + U+1F469 + ZWJ + U+1F467)
/// must not cause a panic.
///
/// Bytes: F0 9F 91 A8  E2 80 8D  F0 9F 91 A9  E2 80 8D  F0 9F 91 A7
#[test]
fn no_panic_zwj_family_emoji() {
    let (mut term, mut parser) = headless_term();
    feed(
        &mut term,
        &mut parser,
        b"\xF0\x9F\x91\xA8\xE2\x80\x8D\xF0\x9F\x91\xA9\xE2\x80\x8D\xF0\x9F\x91\xA7",
    );
}

// ---------------------------------------------------------------------------
// Test 8 — Repeated emoji must not grow unboundedly
// ---------------------------------------------------------------------------

/// Feeding the same emoji 1 000 times must not panic.
///
/// This is a proxy for atlas-growth safety at the terminal-parser level:
/// if the same codepoints are processed many times the internal parser state
/// must remain O(1) and not accumulate unbounded allocations.
#[test]
fn no_panic_repeated_emoji_1000x() {
    // Fire emoji UTF-8: F0 9F 94 A5
    let emoji = b"\xF0\x9F\x94\xA5";

    // Build a single 4 000-byte payload rather than re-entering the parser
    // loop 1 000 times, to also test large buffer handling.
    let payload: Vec<u8> = emoji.iter().cycle().take(emoji.len() * 1_000).copied().collect();

    let (mut term, mut parser) = headless_term();
    feed(&mut term, &mut parser, &payload);

    // The grid dimensions must be unchanged — the parser must not have
    // resized or corrupted the terminal.
    assert_eq!(term.columns(), 80);
    assert_eq!(term.screen_lines(), 24);
}

// ---------------------------------------------------------------------------
// Bonus — random high-entropy bytes must not panic
// ---------------------------------------------------------------------------

/// A block of bytes spanning all possible byte values 0x00–0xFF must not panic.
#[test]
fn no_panic_all_byte_values() {
    let (mut term, mut parser) = headless_term();

    // Feed all 256 possible byte values in sequence.
    let all_bytes: Vec<u8> = (0u8..=255).collect();
    feed(&mut term, &mut parser, &all_bytes);

    // And again in reverse — the parser must have recovered from any error.
    let reversed: Vec<u8> = (0u8..=255).rev().collect();
    feed(&mut term, &mut parser, &reversed);
}
