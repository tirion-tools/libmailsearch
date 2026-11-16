# libmailsearch

Full-text search for a mail client, on Tantivy:

- **`mailsearch-core`** (Rust, `core/`): the rules an index writer and the reader must share. Normalisation N1, the
  `mail` word analyzer, position-preserving bigrams for unsegmented scripts, digit groups and link hosts, the query
  token rules, Snowball stemming, and the index layout with its `VERSION` stamp. The rules are pinned by
  language-neutral test vectors (`tests/vectors/`).
- **`mailsearch`** (Rust, `reader/`, shared library `libmailsearch.so` with the C ABI in `reader/include/mailsearch.h`):
  the search engine over a read-only index.
- **`mailsearch_query`** (C++20, `cpp/`): the query language. The AST, the parser, canonical text and dates.

The library holds no messages and runs no SQL. Results are the caller's stable message ids; the caller resolves them
in its own store and applies the structured filters the parser recognises (flags, folders, sizes) itself.

## Builds on

- **[Tantivy](https://github.com/quickwit-oss/tantivy)** (MIT): the index format, BM25 scoring, FST term dictionaries,
  phrase, prefix and fuzzy queries, and the base word analyzer (`SimpleTokenizer`, `LowerCaser`, `AsciiFoldingFilter`,
  `RemoveLongFilter`). The exact Tantivy version is part of the index stamp.
- **[Snowball](https://snowballstem.org/)** stemming through [frostem](https://crates.io/crates/frostem), the Snowball build
  Tantivy's stemmer uses.
- **[unicode-normalization](https://crates.io/crates/unicode-normalization)** for NFKC.

Fixes to those projects go upstream, not into this repository.

What it adds:

- one versioned set of normalisation rules (NFKC plus removal of Greek, Arabic and Hebrew marks and of invisible and
  bidi characters), pinned by test vectors that both an index writer and the reader run;
- bigrams with positions for Han, kana, Hangul, Thai, Lao, Khmer and Myanmar, so phrase queries work without
  dictionaries (Tantivy's `NgramTokenizer` emits position 0 for every gram);
- address, identifier and phone-number handling (an identifier becomes a phrase of its parts);
- query-time stem expansion against the term dictionary, so the index holds no stems;
- a tiered engine (exact > prefix > stem > typo) with the tier of every hit, a typing path for search as you type
  with early termination, duplicate collapse, an excluded set applied in the collector, people suggestions,
  highlighting, cancellation from another thread, and a reader that refuses any index stamped by other rules;
- a small C ABI that never lets a panic cross it (a panic poisons the handle);
- a query parser compatible with Gmail and eM Client search operators. It never fails, reports a span and hints for
  every token, round-trips to canonical text, and resolves natural-language dates ("last march", "q1 2025",
  "3 weeks ago"), including dates typed among plain words.

Related projects:

- [mail-query](https://github.com/planetaryescape/mail-query) (MIT/Apache-2.0): a strict Rust parser for the Gmail
  operator set with a typed AST. It covers part of this grammar; this parser is kept because it never fails, reports
  spans and hints, accepts eM Client and Outlook keywords and parses natural dates.
- [notmuch](https://notmuchmail.org/) and [mu](https://github.com/djcb/mu): mature Xapian-based mail indexers with their
  own query languages (GPL).
- [lindera-tantivy](https://github.com/lindera/lindera-tantivy), [tantivy-jieba](https://github.com/jiegec/tantivy-jieba),
  [cang-jie](https://github.com/DCjanus/cang-jie): dictionary-based CJK segmentation for Tantivy.
- [charabia](https://github.com/meilisearch/charabia): Meilisearch's multilingual tokenizer.
- [tantivy-analysis-contrib](https://github.com/Dalvany/tantivy-analysis-contrib): ICU and Lucene analysis components
  for Tantivy.
- [seshat](https://github.com/matrix-org/seshat): a Tantivy-based message indexer for Matrix clients.
- [Stalwart](https://stalw.art/docs/storage/fts), [Bichon](https://github.com/rustmailer/bichon): server-side mail
  full-text search (AGPL).

## API

- **Writer side** (`mailsearch-core`): `schema`, `register`, `Fields`, `DocInput` → `to_tantivy`; `people_schema`,
  `PeopleFields`, `PersonInput` / `CorrInput` → `person_doc` / `corr_doc`; `stamp` / `check_stamp`; `read_current`,
  `idx_name`; `NORM_VERSION`, `LAYOUT_VERSION`, `tantivy_version`, `fnv1a64`. The on-disk layout is
  `<root>/CURRENT` naming the live `idx-N/`, each with a `VERSION` stamp, `mail/` and optionally `people/`.
- **Reader** (C ABI, `mailsearch.h`): `mailsearch_versions`, `mailsearch_open`, `mailsearch_set_owner`,
  `mailsearch_set_excluded`, `mailsearch_search`, `mailsearch_cancel`, `mailsearch_people`, `mailsearch_highlight`,
  `mailsearch_reload` / `mailsearch_release` (reader lease), `mailsearch_num_docs`, `mailsearch_close`,
  `mailsearch_last_error`. Queries are typed clause lists (`MailsearchQuery`; clause modes default, exact, phrase, prefix
  and proximity `MAILSEARCH_M_NEAR`); no query string is parsed in Rust. The Rust API is `mailsearch::engine::Engine`.
- **Query language** (C++, namespace `mailsearch`): `parse_query`, `canonical_text`, `same_query`, `remove_chip`,
  `describe_node`, `keyword_completions`, `is_keyword`, and the date helpers in `query_dates.h`. Compiling the AST into
  reader clauses and into the caller's own filters is the caller's job.

Build the shared library with `--profile ffi` (`panic = "unwind"`); a consumer that builds it inside its own workspace
needs a profile with the same setting. Consumers of the C++ part `add_subdirectory(cpp)` and link `mailsearch_query`.

## Security model

- **Queries are hostile input.** The parser bounds every query (4,096 bytes, 256 nodes, depth 16, 32 list items) and
  never fails; the C ABI checks every pointer and length and returns `MAILSEARCH_E_ARG` for invalid UTF-8.
- **Index files are untrusted.** The reader maps the index read-only, refuses an index stamped by another normalisation,
  layout or Tantivy version, and catches every panic at the C ABI (`MAILSEARCH_E_PANIC`, then `MAILSEARCH_E_POISONED`).
  A file truncated under the reader (SIGBUS) is not a panic; the writer's directory layout and the reader lease
  prevent it.
- **Checksums before Tantivy decodes.** Tantivy 0.27 trusts lengths it reads from index files: a corrupted count makes it
  reserve memory up front, and the failed allocation aborts the process (no unwind; upstream report drafted). The
  files a segment decodes when it opens (doc store, fast fields, field norms, delete bitset) are checked against their
  footer CRC32 when the reader first maps them (`mailsearch_core::integrity`); a mismatch fails `mailsearch_open` /
  `mailsearch_reload` with `MAILSEARCH_E_CORRUPT`. A writer calls `integrity::check_index` before it opens segments.
  Cost: one pass over those files per mapping (no stored fields: they are a few bytes per document); term
  dictionaries, postings and positions stay lazily paged.
- **Known limits.** A checksum stops corruption, not a crafted index: a hostile writer can compute it, and Tantivy can
  then still abort the process. A caller that maps an index written by another process keeps a crash guard keyed on
  `mailsearch_current` (the directory to refuse after a crash inside the reader). "At most 3 typo candidates per term"
  is not enforced: the typo tier uses Tantivy's `FuzzyTermQuery` (distance 1, transpositions) as is. Linux and other
  Unix targets (the reader uses `madvise`).

## Tests

- `core/`: unit tests and the rule-by-rule parity test of every vector file in `tests/vectors/` (format in
  `tests/vectors/FORMAT.txt`).
- `reader/tests/abi.rs`: the C ABI over a small index written in the test (versions, refusal, arguments, tiers, dates,
  collapse, exclusion, CJK, typing path, people, highlight, lease, cancel, panic poisoning), and the vectors again.
- `cpp/tests/query_parser_test.cc` (GoogleTest): keyword and alias vectors, operators, dates against a fixed "now",
  the canonical round trip, and the never-fails property on random input.
- `reader/tests/corrupt.rs`: replays the `reader_index` regression inputs (`fuzz/corpus/reader_index/`) through the
  fuzz harness (`reader/tests/corrupt_index/`, shared with the fuzz target); the two Tantivy findings (doc-store
  skip index, fast-field column directory) must return `MAILSEARCH_E_CORRUPT`.
- Fuzz: `fuzz/` (cargo-fuzz `reader_index`: the reader on corrupted index files; seeds and regression inputs in
  `fuzz/corpus/reader_index/`) and `cpp/fuzz/` (libFuzzer `fuzz_query_parser`: parse, round trip, chip edits).

## Gates

Every check runs locally, in a pinned toolchain container (`tools/container/Dockerfile`: Rust 1.93.1 by digest, plus
cargo-fuzz 0.13.2 and cargo-deny 0.20.2, and CMake, Ninja, GoogleTest and clang 19 at exact versions from a dated Debian
snapshot):

```sh
tools/container.sh tools/gate.sh            # fmt, clippy, test, cpp, fuzz, audit
tools/container.sh tools/gate.sh test       # one job
FUZZ_SECONDS=600 tools/container.sh tools/gate.sh fuzz
```

| Job | What |
|---|---|
| `fmt` | `cargo fmt --check` of the workspace and the fuzz harness |
| `clippy` | `cargo clippy --locked --all-targets -D warnings` of both |
| `test` | `cargo test --locked --workspace --profile ffi` |
| `cpp` | the C++ library and its tests with `-Wall -Wextra -Wpedantic -Wshadow -Wconversion -Werror`, then `ctest` |
| `fuzz` | every cargo-fuzz target and the C++ target for `FUZZ_SECONDS` (default 60), with AddressSanitizer |
| `audit` | `cargo deny check`: advisories, licences, bans, sources |

The library ships no binaries. Consumers pin a commit. `.github/workflows/` mirrors the gate for when the repository is
hosted.

## Licence

MIT, see `LICENSE`.
