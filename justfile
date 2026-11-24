set shell := ["bash", "-euo", "pipefail", "-c"]

# Run all static and behavioral gates
check: fmt-check clippy linecount test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets -- -D warnings

linecount:
    python3 tools/check-linecount.py

test:
    cargo test --all-targets

fuzz-smoke:
    cargo test --test fuzz_corpus_smoke

build:
    cargo build --release

track-export:
    /Users/andrin/Code/Euclid/tools/work/work --db .work/switchboard.sqlite3 export --bundle-web --out-dir .work/dashboard

track-serve:
    /Users/andrin/Code/Euclid/tools/work/work --db .work/switchboard.sqlite3 serve --host 127.0.0.1 --port 8766
