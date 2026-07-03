# pdq

Rust-native PDF split and merge MVP.

Runtime constraints:

- does not invoke the `qpdf` binary;
- does not use a subprocess wrapper;
- does not link libqpdf through FFI.

The first implementation uses `lopdf` as a pure-Rust PDF object model and
writer. It focuses on valid split/merge outputs for ordinary, unencrypted PDFs.
Advanced qpdf behavior such as repair, encryption, linearization, forms,
outlines, and full compatibility with unusual historical PDFs is intentionally
out of scope for the current MVP.

## Commands

```sh
cargo test
cargo run --bin pdq -- split input.pdf --out 1-3 out-1.pdf --out 4-z out-2.pdf
cargo run --bin pdq -- split-pages --output 'page-%d.pdf' input.pdf
cargo run --bin pdq -- merge --output merged.pdf a.pdf b.pdf
```

Tests may use `qpdf` as a development validator when it is available on `PATH`.
The runtime implementation must remain qpdf-free.
