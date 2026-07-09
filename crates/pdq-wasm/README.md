# pdq-wasm

Browser/Web Worker bindings for [pdq](../../README.md), generated with
[wasm-bindgen](https://github.com/rustwasm/wasm-bindgen).

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo check -p pdq-wasm --target wasm32-unknown-unknown
```

For npm packaging, run `wasm-pack build crates/pdq-wasm --target web` once
packaging is required.

## API

All binary inputs and outputs are `Uint8Array`. `pages` and range parameters
are strings using pdq's [page range syntax](../../README.md#page-ranges)
(`1-3`, `4-z`, `r1`, `1-3,7,r1`, ...). `password` is `undefined`/omitted for
unencrypted input.

- `version(): string` — the crate version.
- `pageCount(input: Uint8Array, strict: boolean, password?: string): number`
  — page count. `strict: false` trusts the document's `/Count` (fast);
  `strict: true` forces the validated page-tree walk pdq's `split`/
  `split-pages` use, immune to lying metadata.
- `extractTextJson(input: Uint8Array, pages?: string, password?: string): string`
  — positioned text runs for the selected pages (or all pages), returned as
  a JSON string (same shape as `pdq text`).
- `renderPages(input: Uint8Array, pages?: string, dpi?: number): Array<{ page: number, width: number, height: number, png: Uint8Array }>`
  — rasterizes the selected pages (or all pages) to PNG. `dpi` defaults to
  150.
- `split(input: Uint8Array, ranges: string[], password?: string): Array<{ index: number, pdf: Uint8Array }>`
  — one output PDF per range in `ranges`, in the same order, produced in a
  single pass over the input.
- `splitPages(input: Uint8Array, pagesPerFile: number, password?: string): Array<{ index: number, pdf: Uint8Array }>`
  — bursts the document into consecutive chunks of at most `pagesPerFile`
  pages each (`pagesPerFile: 1` gives one PDF per page).
- `merge(inputs: Array<{ pdf: Uint8Array, ranges?: string[] }>, password?: string): Uint8Array`
  — concatenates `inputs` in order into a single PDF. An omitted or empty
  `ranges` includes the whole input; `ranges` (same syntax as above) selects
  a subset of that input's pages. `password` applies to all encrypted
  inputs.

## Usage notes

- **Run on a Web Worker.** `render`, `split`, `splitPages`, and `merge` on
  large PDFs are CPU-bound and can take from tens of milliseconds to
  seconds; calling them on the main thread blocks the UI. Call this module
  from a Web Worker (or behind `postMessage`) and transfer results back to
  the main thread.
- **Everything is in-memory.** There is no filesystem access in Wasm:
  inputs are read fully into a `Uint8Array` and every output (`png`, `pdf`,
  merged bytes) is materialized as a `Uint8Array` held in browser memory at
  once. Rendering many pages at high DPI, or splitting a very large PDF into
  many outputs, can use significant memory — size inputs and outputs with
  the browser's available memory in mind.
- Errors throw a `JsValue` built from the underlying pdq error message
  (e.g. a wrong password, an invalid page range, or a malformed PDF).

## Example

```js
import init, { pageCount, renderPages, split } from "pdq-wasm";

async function run(bytes) {
  await init();

  const pages = pageCount(bytes, false);

  const rendered = renderPages(bytes, "1-3", 150);
  for (const { page, width, height, png } of rendered) {
    // png is a Uint8Array; wrap it in a Blob to display or download.
  }

  const parts = split(bytes, ["1-3", "4-z"]);
  for (const { index, pdf } of parts) {
    // pdf is a Uint8Array containing one of the requested ranges.
  }
}
```
