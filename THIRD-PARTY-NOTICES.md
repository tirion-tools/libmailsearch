# Third-party notices

The code in this repository is MIT-licensed (`LICENSE`, Copyright (c) 2026 Tirion) unless this file says otherwise.

## Bundled third-party code and data

None. The test vectors (`tests/vectors/`), the fuzz seeds and the test messages built inside the tests are synthetic,
written for this repository.

## Dependencies (not bundled)

Dependencies are not vendored: they are fetched at build time (Rust crates from crates.io, pinned in `Cargo.lock`; GoogleTest for the C++ tests) under their own licences, and the gate
checks their licences (`cargo deny`, `deny.toml`). Nothing from them is redistributed in this repository.

- [Tantivy](https://github.com/quickwit-oss/tantivy) (MIT) is the index engine.
