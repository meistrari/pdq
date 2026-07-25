//! Stack headroom for the hayro-backed entry points (`text` and `render`).
//!
//! hayro-syntax's object lexer is mutually recursive — `Object::read` calls
//! `Dict::read`/`Array::read`, which call back into `Object::read` — with no
//! depth cap of any kind (unlike pdq's own parser, whose `MAX_OBJECT_DEPTH`,
//! `MAX_COPY_DEPTH` and `MAX_FORM_RESOURCE_DEPTH` bound every walk, and unlike
//! poppler's `recursionLimit`). A PDF that nests dictionaries or arrays deeply
//! enough therefore overflows the stack, which in Rust is an immediate process
//! abort: no unwinding, no `Result`, exit 134.
//!
//! This is not hypothetical. `corpus/qpdf-qtest/0413-issue-202.pdf` nests its
//! *trailer* dictionary 68,467 levels deep, so `Pdf::new` aborts before any
//! page is touched — while the file is otherwise perfectly readable and
//! extracts correct text once the stack is big enough.
//!
//! Two mitigations live here, and neither is a bound:
//!
//! * [`with_deep_stack`] runs all hayro work on a thread with a large explicit
//!   stack. That covers every real file measured (1,845 corpus PDFs; deepest
//!   is the one above) with a wide margin.
//! * [`guard_nesting_depth`] rejects absurd `<<` nesting visible in the raw
//!   bytes before hayro ever sees them, so an uncompressed dictionary bomb is
//!   a clean error rather than a bigger stack requirement.
//!
//! Two residual holes, both of which still abort the process:
//!
//! * Nesting hidden inside a compressed object stream or content stream. The
//!   depth is invisible until hayro inflates it, and it grows at roughly 1,700
//!   levels per compressed byte, so no stack size can be sound — a 176 KB file
//!   needs 16 GiB. Measured, the smallest reproducer that still aborts is
//!   18 KB (3 million levels in a Flate content stream).
//! * Uncompressed *array* nesting (`[[[[...`). The guard counts `<<` only.
//!   hayro's array reader looks far less stack-hungry than its dictionary
//!   reader — 3-million-level array bombs in the trailer, `/Annots` and
//!   `/Contents` all survive the deep stack — but a 60 MB, 30-million-level
//!   one does abort.
//!
//! The real fix for both is a recursion cap inside hayro-syntax
//! (`Object::read` / `Dict::read` / `Array::read` and their `Skippable::skip`
//! twins), which is not yet filed upstream at LaurenzV/hayro.

use std::path::Path;

use rayon::prelude::*;

use crate::{PdfOpsError, Result};

/// Stack consumed by one level of hayro-syntax's `Object::read` /
/// `dict::read_inner` recursion, rounded up from what was measured on aarch64
/// with hayro-syntax 0.7.2. The cost is dead linear in depth, so this scales
/// [`HAYRO_STACK_SIZE`] directly.
///
/// The dev profile needs its own figure. Unoptimized frames keep every
/// temporary alive and cost ~2.4 KB per level against the optimized build's
/// 488 B — a 5x gap. Deriving one constant from the other used to leave
/// `cargo test` (and every library consumer building in the default profile)
/// accepting files at the cap that then aborted the process, which is exactly
/// the failure [`guard_nesting_depth`] exists to prevent.
const BYTES_PER_NESTING_LEVEL: usize = if cfg!(debug_assertions) { 2560 } else { 512 };

/// Nesting depth [`guard_nesting_depth`] refuses.
///
/// This is the primary constant and [`HAYRO_STACK_SIZE`] is derived from it,
/// so the same file gets the same verdict from a debug and a release build —
/// only the stack the two reserve differs. For scale: the deepest nesting in
/// the 1,845-file corpus is 542 once the known bomb below is excluded, and the
/// bomb itself is 68,467.
#[cfg(target_pointer_width = "64")]
const MAX_NESTING_DEPTH: usize = 262_144;
/// A 32-bit address space cannot spare a multi-hundred-MiB stack per thread,
/// and `render` reserves one per rayon worker, so the cap is far lower there.
/// A file nesting between this and the 64-bit cap is therefore accepted on
/// 64-bit and rejected on 32-bit — including
/// `corpus/qpdf-qtest/0413-issue-202.pdf`.
#[cfg(not(target_pointer_width = "64"))]
const MAX_NESTING_DEPTH: usize = 4_096;

/// Stack for the thread hayro runs on: twice what [`MAX_NESTING_DEPTH`] levels
/// cost, so the guard always rejects before the stack runs out and the 2x gap
/// absorbs drift in the per-level cost across hayro versions and
/// architectures. 256 MiB on an optimized 64-bit build.
///
/// The reservation itself is nearly free — thread stacks are lazily committed,
/// so the spawn is ~50 us and untouched pages never reach RSS. Actually
/// *using* it is not free: a file that needs 300,000 levels really does commit
/// ~150 MiB, so the fix trades an abort for a large transient allocation.
pub(crate) const HAYRO_STACK_SIZE: usize = 2 * MAX_NESTING_DEPTH * BYTES_PER_NESTING_LEVEL;

/// Run `f` on a thread with [`HAYRO_STACK_SIZE`] of stack.
///
/// Scoped so the closure can borrow its arguments instead of being forced to
/// `'static`. A panic inside `f` is re-raised on the calling thread rather
/// than converted to an error: callers (and `tests/corpus.rs`) classify
/// panics differently from failures, and laundering them here would hide
/// every future hayro panic behind a benign-looking `Err`.
pub(crate) fn with_deep_stack<T, F>(f: F) -> Result<T>
where
    T: Send,
    F: FnOnce() -> Result<T> + Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("pdq-hayro".to_string())
            .stack_size(HAYRO_STACK_SIZE)
            .spawn_scoped(scope, f)?;
        handle
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// Reject a file whose *literal* `<<`/`>>` nesting exceeds what the hayro
/// thread's stack can survive, so it fails as an error instead of aborting.
///
/// The scan pairs tokens greedily and clamps the running depth at zero, which
/// makes it safe in the only direction that matters. Stray `<<`/`>>` bytes
/// inside binary stream payloads can only *inflate* the measurement, because
/// they occur where the real lexical depth is near zero and the clamp stops
/// unbalanced `>>` runs from cancelling a genuine deep run elsewhere. So the
/// check can in principle false-reject, never false-accept — and the margin
/// against false rejection is enormous (corpus maximum 542 against a cap of
/// [`MAX_NESTING_DEPTH`]).
///
/// It counts `<<` only. Array nesting (`[[[[...`) is invisible to it, as is
/// any nesting inside a Flate-compressed object stream or content stream; see
/// the module docs.
pub(crate) fn guard_nesting_depth(data: &[u8], input: &Path) -> Result<()> {
    // The depth can never exceed the number of `<<` digraphs, so counting
    // those decides almost every input without the exact scan below ever
    // running. It is worth the split: the exact scan has to stop at every one
    // of a large PDF's angle brackets (2.1 million in the 200 MB fixture) and
    // cost 30 ms of pure prepass there before a single page was touched, while
    // the count is a chunkable memchr sweep that stays near memory bandwidth.
    if count_open_digraphs(data) <= MAX_NESTING_DEPTH {
        return Ok(());
    }

    let mut depth: usize = 0;
    let mut consumed = 0usize;
    for at in memchr::memchr2_iter(b'<', b'>', data) {
        // Second byte of a token already consumed by the previous iteration.
        if at < consumed {
            continue;
        }
        let byte = data[at];
        if data.get(at + 1) != Some(&byte) {
            continue;
        }
        consumed = at + 2;
        if byte == b'<' {
            depth += 1;
            if depth > MAX_NESTING_DEPTH {
                return Err(PdfOpsError::Unsupported(format!(
                    "object nesting depth exceeds {MAX_NESTING_DEPTH} in {}",
                    input.display()
                )));
            }
        } else {
            depth = depth.saturating_sub(1);
        }
    }
    Ok(())
}

/// Number of positions where `<` is followed by `<`, counting overlaps: `<<<`
/// is two.
///
/// That over-counts the depth scan's tokens, which pair greedily and see one
/// token in `<<<` — deliberately, because over-counting is the safe direction
/// and it makes the count chunkable. Every token the scan finds sits at a
/// distinct position that is also a digraph here, so this is an upper bound on
/// the depth the scan can reach.
///
/// Each chunk owns the positions inside it and reads one byte past its end,
/// so chunking changes nothing about the result — only how fast it arrives.
fn count_open_digraphs(data: &[u8]) -> usize {
    /// Below this, one thread beats the cost of waking rayon's pool.
    const PARALLEL_MIN_LEN: usize = 4 << 20;

    if data.len() < PARALLEL_MIN_LEN {
        return count_open_digraphs_in(data, 0, data.len());
    }

    let chunk = data.len().div_ceil(rayon::current_num_threads().max(1));
    (0..data.len().div_ceil(chunk))
        .into_par_iter()
        .map(|index| {
            let start = index * chunk;
            count_open_digraphs_in(data, start, (start + chunk).min(data.len()))
        })
        .sum()
}

fn count_open_digraphs_in(data: &[u8], start: usize, end: usize) -> usize {
    memchr::memchr_iter(b'<', &data[start..end])
        .filter(|offset| data.get(start + offset + 1) == Some(&b'<'))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth_error(data: &[u8]) -> Option<String> {
        guard_nesting_depth(data, Path::new("x.pdf"))
            .err()
            .map(|err| err.to_string())
    }

    #[test]
    fn ordinary_nesting_is_accepted() {
        let data = b"trailer\n<</Size 4/Root<</Type/Catalog/Pages<</Count 1>>>>>>\n";
        assert!(depth_error(data).is_none());
    }

    #[test]
    fn deep_nesting_is_rejected_before_parsing() {
        let mut data = b"trailer\n".to_vec();
        data.extend(std::iter::repeat_n(b'<', 2 * (MAX_NESTING_DEPTH + 1)));
        data.extend_from_slice(b"/Size 4");
        data.extend(std::iter::repeat_n(b'>', 2 * (MAX_NESTING_DEPTH + 1)));
        let message = depth_error(&data).expect("deep nesting must be rejected");
        assert!(message.contains("nesting depth"), "{message}");
    }

    /// Nesting that opens and closes repeatedly is ordinary PDF structure and
    /// must stay acceptable, while a deep run appearing after it must still
    /// be caught.
    #[test]
    fn closed_nesting_is_accepted_and_a_later_deep_run_is_not() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend(std::iter::repeat_n(b'<', 2 * (MAX_NESTING_DEPTH / 2)));
            data.extend(std::iter::repeat_n(b'>', 2 * (MAX_NESTING_DEPTH / 2)));
        }
        assert!(depth_error(&data).is_none());
        data.extend(std::iter::repeat_n(b'<', 2 * (MAX_NESTING_DEPTH + 1)));
        assert!(depth_error(&data).is_some());
    }

    /// Unbalanced `>>` in binary stream data clamps at zero rather than going
    /// negative, so it cannot cancel out real nesting that follows.
    #[test]
    fn unbalanced_close_tokens_clamp_at_zero() {
        let mut data = std::iter::repeat_n(b'>', 4096).collect::<Vec<u8>>();
        data.extend(std::iter::repeat_n(b'<', 2 * (MAX_NESTING_DEPTH + 1)));
        assert!(depth_error(&data).is_some());
    }

    /// The chunked count must agree with the single-threaded one byte for
    /// byte, including where a digraph straddles a chunk boundary — a chunk
    /// that under-counts would let the exact scan be skipped on a real bomb.
    #[test]
    fn chunked_and_sequential_digraph_counts_agree() {
        let mut data = Vec::with_capacity(8 << 20);
        let pattern: &[u8] = b"<< /A 1 <<>> <<< >>> x < > <<";
        while data.len() < (8 << 20) {
            data.extend_from_slice(pattern);
            data.push(b'<');
        }
        let sequential = count_open_digraphs_in(&data, 0, data.len());
        assert_eq!(count_open_digraphs(&data), sequential);
        // Force boundaries at every offset of a small window to catch the
        // straddling case directly.
        let window = &data[..1024];
        for split in 1..window.len() {
            let halves = count_open_digraphs_in(window, 0, split)
                + count_open_digraphs_in(window, split, window.len());
            assert_eq!(
                halves,
                count_open_digraphs_in(window, 0, window.len()),
                "split at {split}"
            );
        }
    }

    /// A run of `n` `<` bytes is `n - 1` digraphs but only `n / 2` tokens for
    /// the exact scan, so the cheap count must never be the smaller of the two.
    #[test]
    fn digraph_count_bounds_the_exact_scan() {
        for run in 1..64usize {
            let data = vec![b'<'; run];
            let digraphs = count_open_digraphs(&data);
            assert!(
                digraphs >= run / 2,
                "run of {run}: {digraphs} digraphs under-counts {} tokens",
                run / 2
            );
        }
    }

    #[test]
    fn deep_stack_propagates_results_and_panics() {
        assert_eq!(with_deep_stack(|| Ok(7)).unwrap(), 7);
        let panicked =
            std::panic::catch_unwind(|| with_deep_stack(|| -> Result<()> { panic!("boom") }));
        assert!(panicked.is_err(), "a panic must not be laundered into Err");
    }
}
