// Date values and phrases of the query language. Calendar days in the local time
// zone (the process TZ); weeks start on Monday.
#pragma once

#include <array>
#include <cstdint>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "mailsearch/query_ast.h"

namespace mailsearch {

struct DateOptions {
  int64_t now = 0;  // unix seconds: relative dates resolve against it
  // Numeric dates whose day/month order is ambiguous (07/05/2024): true = day first, from the
  // user's LC_TIME; a part above 12 decides regardless.
  bool day_first = true;
  // The UI locale's month (January first) and weekday (Monday first) names, lowercased, in addition
  // to English (the app fills them from ICU). Empty = English only.
  std::array<std::vector<std::string>, 12> locale_months;
  std::array<std::vector<std::string>, 7> locale_weekdays;
};

// [lo, hi) in unix seconds; kOpenFrom / kOpenTo for open ends.
struct Period {
  int64_t lo = kOpenFrom, hi = kOpenTo;
  bool operator==(const Period&) const = default;
};

// A whole keyword value (`before:2024-07-05`, `date:"last march"`, `within:2d`,
// `date:2024-03-01..2024-03-15`): absolute, natural, duration or a `..` range. nullopt = not a date.
std::optional<Period> parse_date_value(std::string_view value, const DateOptions& options);

// The longest date phrase that starts at words[0] in bare text ("invoice last march"): returns the
// number of words consumed (0 = none) and *out. Stricter than keyword values, so ordinary words stay
// text: no bare year, no durations, no lone `may` or abbreviated month/weekday names.
size_t match_date_phrase(std::span<const std::string_view> words, const DateOptions& options, Period* out);

// The DateRange a keyword applies: on = the period, before = [open, lo), after = [lo, open).
Period apply_date_op(DateOp op, const Period& p);

// Chip text with the interpreted range, e.g. "before 5 Jul 2024", "March 2026", "since 1 Mar 2024",
// "2 Mar 2026 – 8 Mar 2026" (English month abbreviations; the UI localises around it).
std::string describe_range(int64_t from, int64_t to);

}  // namespace mailsearch
