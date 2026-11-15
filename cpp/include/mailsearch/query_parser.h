// Query text ⇄ AST. parse_query never fails: unknown or malformed syntax
// degrades to free-text terms with a "treated as text" hint, recognised-but-unsupported keywords
// become "not available" Filter chips, and input past the bounds of query_ast.h is ignored with a
// kTruncated hint. canonical_text(parse_query(t).query) re-parses to the same tree (same_query).
#pragma once

#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

#include "mailsearch/query_ast.h"
#include "mailsearch/query_dates.h"

namespace mailsearch {

struct ParseOptions {
  DateOptions dates;
  // Date phrases in bare text become DateRange chips.
  bool bare_dates = true;
};

// `text` is sanitized first (invalid UTF-8 → U+FFFD, control characters → space, cut at
// kMaxQueryBytes on a character boundary); spans index the sanitized text, returned in *sanitized
// when non-null.
ParseResult parse_query(std::string_view text, const ParseOptions& options, std::string* sanitized = nullptr);

// Canonical text of the tree (canonical keyword names, aliases folded, `OR` between alternatives,
// date chips as `date:"<source text>"`). Scope, Sort and DateHint are not part of it.
std::string canonical_text(const Query& q, const ParseOptions& options);

// Chip edits. Removing a DateRange keeps its range as the query's DateHint;
// removing any other chip drops its subtree (a Not or a one-child group around it goes too).
Query remove_chip(const Query& q, uint32_t node);

// Chip label for a node: "from: john", "before 5 Jul 2024", "is unread", "tag: Invoices",
// "notes: not available", …
std::string describe_node(const Query& q, uint32_t node);

// Keyword names (canonical and aliases, with the colon) starting with `prefix`, case-insensitive,
// for the keyword list and completions.
std::vector<std::string_view> keyword_completions(std::string_view prefix);

// True for keyword names this grammar knows (canonical, alias or not-available), lowercase input.
bool is_keyword(std::string_view lowercase_name);

}  // namespace mailsearch
