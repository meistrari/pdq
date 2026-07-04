#!/usr/bin/env bash
# Populate corpus/ with real PDFs for tests/corpus.rs.
#
# Usage:
#   scripts/fetch_corpus.sh --local DIR   # symlink every PDF under DIR
#   scripts/fetch_corpus.sh --qpdf        # qpdf's qtest suite (~hundreds, many intentionally weird)
#   scripts/fetch_corpus.sh --pdfjs       # Mozilla pdf.js test corpus (real-world PDFs)
#
# Flags can be combined. The corpus lives in corpus/ (gitignored); local
# files are symlinked, so nothing is duplicated or leaves the machine.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p corpus

if [ $# -eq 0 ]; then
    grep '^#' "$0" | head -12
    exit 1
fi

while [ $# -gt 0 ]; do
    case "$1" in
    --local)
        src="${2:?--local needs a directory}"
        shift 2
        dest="corpus/local"
        mkdir -p "$dest"
        i=0
        while IFS= read -r -d '' f; do
            i=$((i + 1))
            ln -sf "$f" "$dest/$(printf '%04d' "$i")-$(basename "$f")"
        done < <(find "$src" -maxdepth 3 -name '*.pdf' -type f -print0)
        echo "local: linked $i PDFs from $src into $dest"
        ;;
    --qpdf)
        shift
        clone=$(mktemp -d)
        git clone --depth 1 --quiet https://github.com/qpdf/qpdf "$clone/qpdf"
        dest="corpus/qpdf-qtest"
        mkdir -p "$dest"
        i=0
        while IFS= read -r -d '' f; do
            i=$((i + 1))
            cp "$f" "$dest/$(printf '%04d' "$i")-$(basename "$f")"
        done < <(find "$clone/qpdf/qpdf/qtest" -name '*.pdf' -type f -print0)
        rm -rf "$clone"
        echo "qpdf: copied $i PDFs into $dest"
        ;;
    --pdfjs)
        shift
        clone=$(mktemp -d)
        git clone --depth 1 --filter=blob:none --sparse --quiet \
            https://github.com/mozilla/pdf.js "$clone/pdf.js"
        git -C "$clone/pdf.js" sparse-checkout set test/pdfs
        dest="corpus/pdfjs"
        mkdir -p "$dest"
        i=0
        while IFS= read -r -d '' f; do
            i=$((i + 1))
            cp "$f" "$dest/$(basename "$f")"
        done < <(find "$clone/pdf.js/test/pdfs" -name '*.pdf' -type f -print0)
        rm -rf "$clone"
        echo "pdfjs: copied $i PDFs into $dest"
        ;;
    *)
        echo "unknown flag: $1" >&2
        exit 1
        ;;
    esac
done

total=$(find corpus -name '*.pdf' | wc -l | tr -d ' ')
echo "corpus now holds $total PDFs; run: cargo test --release --test corpus -- --nocapture"
