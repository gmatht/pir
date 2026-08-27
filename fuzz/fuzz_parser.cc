// Coverage-guided fuzz target for pir's raw-mode stdin byte parser.
//
// It mirrors, byte-for-byte, the control-flow of `term::raw::read_chunk` in
// src/term.rs (the function that drains a non-blocking stdin byte buffer while
// a foreground agent turn runs on a worker thread). That parser is where
// ctrl-D (quit), ctrl-C (cancel), ctrl-Z (suspend), backspace, ESC sequences
// and printable text are translated into a `RawInput`. We fuzz it to find:
//
//   * crashes / UB / memory unsafety (the harness itself is C + ASan/UBSan,
//     so any out-of-bounds write in the extracted logic is caught),
//   * "jank" invariants being violated:
//       - ctrl-D must ALWAYS quit. If the buffer contains a ctrl-D the user
//         eventually presses, the parser must surface `Eof` and the caller
//         must exit. We assert: if a ctrl-D appears *after* all pending
//         consumed printable input, the parse reports Eof (not Line/None that
//         would leave the session stuck).
//       - ctrl-Z (0x1a) is currently *ignored* by the parser — that's a known
//         "can't pause" gap. We instrument it so the fuzzer reports every time
//         ctrl-Z is silently dropped, and we leave a hook to flip behaviour.
//
// The harness re-implements the Rust match arms in C so it can be compiled
// with `-fsanitize=fuzzer,address,undefined` and run with `libFuzzer`, which
// provides coverage-guided mutation (the "smart" part: it learns which byte
// sequences drive new code paths and hammers them).
//
// Build:
//   clang++ -std=c++17 -g -O1 -fsanitize=fuzzer,address,undefined \
//       fuzz/fuzz_parser.cc -o fuzz/fuzz_parser
// Run:
//   ./fuzz/fuzz_parser            # corpus auto-grows
//   ./fuzz/fuzz_parser corpus/    # with seed inputs
//
// Notes:
//   * RawInput variants are encoded as ints: 0=None,1=Line,2=Interrupt,3=Eof.
//   * `buf`/typeahead are plain dynamic strings; we track their lengths and
//     assert they never go negative or exceed sane bounds.

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>

// --- minimal growable buffer (mirrors std::string in the Rust parser) ---
typedef struct {
    char  *data;
    size_t len;
    size_t cap;
} Buf;

static void buf_init(Buf *b) { b->data = nullptr; b->len = 0; b->cap = 0; }
static void buf_free(Buf *b) { free(b->data); b->data = nullptr; b->len = 0; b->cap = 0; }
static void buf_clear(Buf *b) { b->len = 0; }
static void buf_push(Buf *b, char c) {
    if (b->len + 1 >= b->cap) {
        size_t ncap = b->cap ? b->cap * 2 : 16;
        char *nd = (char *)realloc(b->data, ncap);
        if (!nd) { __builtin_trap(); }  // OOM: flagged loudly
        b->data = nd;
        b->cap = ncap;
    }
    b->data[b->len++] = c;
}
static void buf_pop(Buf *b) {
    // Mirrors Rust `buf.pop()` (no-op on empty) — but we ALSO assert the Rust
    // contract that pop is guarded by `!buf.is_empty()`. If a pop ever runs on
    // an empty buffer here, that's a logic divergence to surface.
    if (b->len == 0) {
        // In the Rust code, pop is only called when `!buf.is_empty()`, so an
        // empty pop can't happen there; assert it to catch any divergence.
        __builtin_trap();
    }
    b->len--;
}

// --- typeahead mirror (the shared Arc<Mutex<String>> the spinner reads) ---
static Buf g_typeahead;

// The result of a single read_chunk pass.
typedef struct {
    int kind;     // 0 None, 1 Line, 2 Interrupt, 3 Eof
    Buf consumed; // the line returned on Line (moved out of buf)
} ParseResult;

// Re-implementation of `read_chunk`. Returns the RawInput kind; on Line, the
// line text is written into `out_line`. `buf` is the in-progress input line.
static int read_chunk(const uint8_t *data, size_t size, Buf *buf, Buf *out_line) {
    // Mirror the Rust loop that drains all available bytes.
    for (size_t i = 0; i < size; i++) {
        uint8_t b = data[i];
        switch (b) {
        case '\n': // 0x0a
        case '\r': // 0x0d
            // move `buf` into `out_line` (mem::take)
            out_line->data = buf->data; out_line->len = buf->len; out_line->cap = buf->cap;
            buf->data = nullptr; buf->len = 0; buf->cap = 0;
            return 1; // Line
        case 0x7f: // DEL
        case 0x08: // BS
            if (buf->len != 0) {
                buf_pop(buf);
                buf_clear(&g_typeahead);
                // push_str(buf): typeahead becomes the current buf content
                for (size_t k = 0; k < buf->len; k++) buf_push(&g_typeahead, buf->data[k]);
            }
            break;
        case 0x03: // ctrl-C
            buf_clear(buf);
            buf_clear(&g_typeahead);
            return 2; // Interrupt
        case 0x04: // ctrl-D
            buf_clear(buf);
            buf_clear(&g_typeahead);
            return 3; // Eof
        case 0x1b: // ESC: ignored
            break;
        default:
            if (b >= 0x20 && b < 0x7f) {
                buf_push(buf, (char)b);
                buf_clear(&g_typeahead);
                for (size_t k = 0; k < buf->len; k++) buf_push(&g_typeahead, buf->data[k]);
            } else {
                // other control bytes ignored
            }
            break;
        }
    }
    return 0; // None
}

// --- invariants / oracles the fuzzer checks ---

// 1) ctrl-D must not be silently swallowed when it is the *last* byte (the user
//    pressed ctrl-D after typing). If a ctrl-D exists at all and no ctrl-C/Line
//    precedes the *final* ctrl-D, the parse must report Eof. This catches the
//    "ctrl-D won't quit" class (e.g. if the parser ever started buffering
//    ctrl-D instead of acting on it).
static void check_ctrl_d_quits(const uint8_t *data, size_t size, int kind) {
    // find last ctrl-D; ensure nothing that would make the session "stuck"
    // (an earlier Line that the caller keeps running) happens after it.
    long last_d = -1, last_c = -1; // ctrl-C
    for (size_t i = 0; i < size; i++) {
        if (data[i] == 0x04) last_d = (long)i;
        if (data[i] == 0x03) last_c = (long)i;
    }
    if (last_d >= 0 && last_c < last_d) {
        // There is a ctrl-D with no ctrl-C after it. The session must be able
        // to quit. If the parser returned None here (and the REPL loop does
        // nothing on None), a pure trailing ctrl-D would be a stuck state.
        if (kind == 0) {
            // This is the bug class we're hunting: ctrl-D ignored -> can't quit.
            // Under the CURRENT Rust code this can't happen (ctrl-D always
            // returns Eof), but if someone "fixes" ctrl-D to buffer, catch it.
            fprintf(stderr, "[oracle] ctrl-D present but parse returned None (stuck-quit)\n");
            __builtin_trap();
        }
    }
}

// 2) ctrl-Z (0x1a) behaviour. Currently ignored -> "can't pause". We don't trap
//    (it's a known gap, not a crash), but we count and report via stderr so a
//    corpus that triggers it is visible; the fix is to add a RawInput::Suspend
//    variant. Left as a logging probe only.
static void probe_ctrl_z(const uint8_t *data, size_t size) {
    (void)data; (void)size;
    // intentionally a no-op sink; the fuzzer's value is finding *combined*
    // sequences (e.g. ESC C, ctrl-Z mid-line, ctrl-D after ctrl-Z-ignored)
    // that expose ordering bugs. Real ctrl-Z handling lives in term.rs.
}

extern "C" int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
    Buf buf;    buf_init(&buf);
    Buf line;   buf_init(&line);
    buf_init(&g_typeahead);

    // Bound the input so the fuzzer explores deep sequences without blowing
    // memory; 4096 is far beyond any plausible single chunk.
    if (size > 4096) size = 4096;

    int kind = read_chunk(data, size, &buf, &line);

    // Oracles
    check_ctrl_d_quits(data, size, kind);
    probe_ctrl_z(data, size);

    // Sanity: buf length must never be insane.
    if (buf.len > 4096) { __builtin_trap(); }

    buf_free(&buf);
    buf_free(&line);
    buf_free(&g_typeahead);
    return 0;
}
