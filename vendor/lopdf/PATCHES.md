# Local lopdf patch

Source: crates.io `lopdf` 0.43.0 (MIT; see LICENSE).
The source, examples and tests are copied unchanged except for the patch below.

In `src/parser/mod.rs`, `stream()` now accepts PDF whitespace between the
`/Length`-delimited payload and `endstream`, instead of only one optional EOL.
Scanner PDFs can indent `endstream` with a tab. Previously stream recognition
failed, the indirect-object parser accepted just the dictionary, and pdq copied
an object without its drawing commands. The operation succeeded with blank pages.

The declared payload length is still authoritative: no payload bytes are trimmed
or scanned for keywords. `tests/split_merge.rs` in pdq verifies preservation of
stream bytes and rendered output, including direct and indirect lengths.

Remove this local patch when adopting an upstream release with the same fix.
