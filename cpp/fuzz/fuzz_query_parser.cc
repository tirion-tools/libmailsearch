// libFuzzer target for the query language: parse, canonical round trip, chip edits and labels. Byte 0
// selects options (day-first dates, bare dates); the rest is the query text. Invariants checked beyond
// "no crash / no sanitizer report": the node cap holds, every node has a span, and the canonical
// round trip is the identity.
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <string>
#include <string_view>

#include "mailsearch/query_parser.h"

namespace s = mailsearch;

namespace {

void check(bool ok) {
  if (!ok) __builtin_trap();
}

}  // namespace

extern "C" int LLVMFuzzerInitialize(int* /*argc*/, char*** /*argv*/) {
  setenv("TZ", "CET-1CEST,M3.5.0,M10.5.0/3", 1);
  return 0;
}

extern "C" int LLVMFuzzerTestOneInput(const uint8_t* data, size_t size) {
  if (size == 0) return 0;
  const uint8_t mode = data[0];
  const std::string_view text(reinterpret_cast<const char*>(data + 1), size - 1);
  s::ParseOptions opts;
  opts.dates.now = 1794657600;  // fixed: findings must reproduce
  opts.dates.day_first = (mode & 1) != 0;
  opts.bare_dates = (mode & 4) == 0;

  const s::ParseResult a = s::parse_query(text, opts);
  size_t leaves = 0;
  for (const s::Node& n : a.query.nodes) leaves += (n.kind != s::NodeKind::kAnd && n.kind != s::NodeKind::kOr) ? 1 : 0;
  check(leaves <= s::kMaxNodes);
  check(a.spans.size() == a.query.nodes.size());

  const std::string canonical = s::canonical_text(a.query, opts);
  if (canonical.size() <= s::kMaxQueryBytes) {
    const s::ParseResult b = s::parse_query(canonical, opts);
    check(s::same_query(a.query, b.query));
  }
  for (uint32_t i = 0; i < a.query.nodes.size() && i < 8; ++i) {
    (void)s::describe_node(a.query, i);
    (void)s::canonical_text(s::remove_chip(a.query, i), opts);
  }
  return 0;
}
