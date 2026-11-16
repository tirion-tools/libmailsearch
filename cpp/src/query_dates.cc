#include "mailsearch/query_dates.h"

#include <algorithm>
#include <charconv>
#include <ctime>

namespace mailsearch {
namespace {

constexpr int kMinYear = 1970;
constexpr int kMaxYear = 2200;
constexpr int kMaxAgo = 10000;  // N in `N days ago` / `2d`

constexpr std::array<std::string_view, 12> kMonths = {"january", "february", "march",     "april",   "may",      "june",
                                                      "july",    "august",   "september", "october", "november", "december"};
constexpr std::array<std::string_view, 12> kMonthTitle = {"January", "February", "March",     "April",   "May",      "June",
                                                          "July",    "August",   "September", "October", "November", "December"};
constexpr std::array<std::string_view, 12> kMonthAbbr = {"Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                                         "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"};
constexpr std::array<std::string_view, 7> kWeekdays = {"monday", "tuesday", "wednesday", "thursday",
                                                       "friday", "saturday", "sunday"};

struct Today {
  int y = 1970, m = 1, d = 1;
  int wd = 0;  // Monday = 0
};

Today today_of(int64_t now) {
  const time_t t = static_cast<time_t>(now);
  std::tm tmv{};
  localtime_r(&t, &tmv);
  Today out;
  out.y = tmv.tm_year + 1900;
  out.m = tmv.tm_mon + 1;
  out.d = tmv.tm_mday;
  out.wd = (tmv.tm_wday + 6) % 7;
  return out;
}

// Local midnight of y-m-d; out-of-range months and days normalise (mktime), so m + 1 or d - 7 work.
int64_t midnight(int y, int m, int d) {
  std::tm t{};
  t.tm_year = y - 1900;
  t.tm_mon = m - 1;
  t.tm_mday = d;
  t.tm_isdst = -1;
  return static_cast<int64_t>(std::mktime(&t));
}

Period day_period(int y, int m, int d) { return {midnight(y, m, d), midnight(y, m, d + 1)}; }
Period month_period(int y, int m) { return {midnight(y, m, 1), midnight(y, m + 1, 1)}; }
Period year_period(int y) { return {midnight(y, 1, 1), midnight(y + 1, 1, 1)}; }

bool leap(int y) { return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0; }
int days_in_month(int y, int m) {
  static constexpr std::array<int, 12> kDays = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
  return m == 2 && leap(y) ? 29 : kDays[static_cast<size_t>(m - 1)];
}

std::string lower_ascii(std::string_view s) {
  std::string out(s);
  for (char& c : out) {
    if (c >= 'A' && c <= 'Z') c = static_cast<char>(c - 'A' + 'a');
  }
  return out;
}

// A word as the matcher sees it: ASCII-lowercased, trailing ',' and '.' removed ("July 5, 2024").
std::string norm_word(std::string_view w) {
  while (!w.empty() && (w.back() == ',' || w.back() == '.')) w.remove_suffix(1);
  return lower_ascii(w);
}

std::optional<int> parse_int(std::string_view s, size_t min_digits, size_t max_digits) {
  if (s.size() < min_digits || s.size() > max_digits) return std::nullopt;
  int v = 0;
  for (char c : s) {
    if (c < '0' || c > '9') return std::nullopt;
  }
  const auto r = std::from_chars(s.data(), s.data() + s.size(), v);
  if (r.ec != std::errc() || r.ptr != s.data() + s.size()) return std::nullopt;
  return v;
}

std::optional<int> parse_year(std::string_view s) {
  auto y = parse_int(s, 4, 4);
  if (!y || *y < kMinYear || *y > kMaxYear) return std::nullopt;
  return y;
}

// 1..31, optionally with an English ordinal suffix ("5th").
std::optional<int> parse_day(std::string_view s) {
  for (std::string_view suffix : {"st", "nd", "rd", "th"}) {
    if (s.size() > suffix.size() && s.ends_with(suffix)) {
      s.remove_suffix(suffix.size());
      break;
    }
  }
  auto d = parse_int(s, 1, 2);
  if (!d || *d < 1 || *d > 31) return std::nullopt;
  return d;
}

// 1..12 or 0. Abbreviations: the first three letters, and "sept".
int month_index(std::string_view w, bool allow_abbrev, const DateOptions& o) {
  for (size_t i = 0; i < kMonths.size(); ++i) {
    if (w == kMonths[i]) return static_cast<int>(i) + 1;
    if (allow_abbrev && w.size() == 3 && kMonths[i].starts_with(w)) return static_cast<int>(i) + 1;
    for (const std::string& n : o.locale_months[i]) {
      if (!n.empty() && w == n) return static_cast<int>(i) + 1;
    }
  }
  if (allow_abbrev && w == "sept") return 9;
  return 0;
}

// 1..7 (Monday = 1) or 0.
int weekday_index(std::string_view w, bool allow_abbrev, const DateOptions& o) {
  for (size_t i = 0; i < kWeekdays.size(); ++i) {
    if (w == kWeekdays[i]) return static_cast<int>(i) + 1;
    if (allow_abbrev && w.size() == 3 && kWeekdays[i].starts_with(w)) return static_cast<int>(i) + 1;
    for (const std::string& n : o.locale_weekdays[i]) {
      if (!n.empty() && w == n) return static_cast<int>(i) + 1;
    }
  }
  return 0;
}

std::optional<Period> checked_day(int y, int m, int d) {
  if (y < kMinYear || y > kMaxYear || m < 1 || m > 12 || d < 1 || d > days_in_month(y, m)) return std::nullopt;
  return day_period(y, m, d);
}

// A day without a year: its most recent occurrence up to today.
std::optional<Period> yearless_day(int m, int d, const Today& t) {
  const int y = (m > t.m || (m == t.m && d > t.d)) ? t.y - 1 : t.y;
  return checked_day(y, m, d);
}

// One-token absolute dates: 2024-07-05, 2024/07/05, 2024-07, 2024/07, and numeric 07/05/2024,
// 07.05.2024, 07-05-2024 (day/month order from LC_TIME unless a part exceeds 12).
std::optional<Period> numeric_token(std::string_view w, const DateOptions& o) {
  char sep = 0;
  for (char c : w) {
    if (c == '-' || c == '/' || c == '.') {
      sep = c;
      break;
    }
  }
  if (sep == 0) return std::nullopt;
  std::array<std::string_view, 3> parts{};
  size_t n = 0;
  size_t start = 0;
  for (size_t i = 0; i <= w.size(); ++i) {
    if (i == w.size() || w[i] == sep) {
      if (n == parts.size()) return std::nullopt;
      parts[n++] = w.substr(start, i - start);
      start = i + 1;
    } else if (w[i] < '0' || w[i] > '9') {
      return std::nullopt;
    }
  }
  if (n < 2) return std::nullopt;
  if (parts[0].size() == 4) {  // ISO order
    if (sep == '.') return std::nullopt;
    auto y = parse_year(parts[0]);
    auto m = parse_int(parts[1], 1, 2);
    if (!y || !m || *m < 1 || *m > 12) return std::nullopt;
    if (n == 2) return month_period(*y, *m);
    auto d = parse_int(parts[2], 1, 2);
    if (!d) return std::nullopt;
    return checked_day(*y, *m, *d);
  }
  if (n != 3) return std::nullopt;
  auto a = parse_int(parts[0], 1, 2);
  auto b = parse_int(parts[1], 1, 2);
  auto y = parse_year(parts[2]);
  if (!a || !b || !y) return std::nullopt;
  bool day_first = o.day_first;
  if (*a > 12 && *b <= 12) day_first = true;
  if (*b > 12 && *a <= 12) day_first = false;
  return day_first ? checked_day(*y, *b, *a) : checked_day(*y, *a, *b);
}

enum class Unit : uint8_t { kNone, kDay, kWeek, kMonth, kYear };

Unit unit_word(std::string_view w) {
  if (w == "day" || w == "days") return Unit::kDay;
  if (w == "week" || w == "weeks") return Unit::kWeek;
  if (w == "month" || w == "months") return Unit::kMonth;
  if (w == "year" || w == "years") return Unit::kYear;
  return Unit::kNone;
}

// Gmail `2d` `3w` `6m` `1y`; eM Client `1Day` `2Weeks` `1Month` `1Year` (already lowercased).
std::optional<std::pair<int, Unit>> duration_token(std::string_view w) {
  size_t i = 0;
  while (i < w.size() && w[i] >= '0' && w[i] <= '9') ++i;
  if (i == 0 || i == w.size()) return std::nullopt;
  auto n = parse_int(w.substr(0, i), 1, 5);
  if (!n || *n < 1 || *n > kMaxAgo) return std::nullopt;
  const std::string_view u = w.substr(i);
  Unit unit = unit_word(u);
  if (u == "d") unit = Unit::kDay;
  if (u == "w") unit = Unit::kWeek;
  if (u == "m") unit = Unit::kMonth;
  if (u == "y") unit = Unit::kYear;
  if (unit == Unit::kNone) return std::nullopt;
  return std::pair{*n, unit};
}

// From the start of the day N units back to the end of today.
std::optional<Period> duration_period(int n, Unit u, const Today& t) {
  int64_t lo = 0;
  switch (u) {
    case Unit::kDay: lo = midnight(t.y, t.m, t.d - n); break;
    case Unit::kWeek: lo = midnight(t.y, t.m, t.d - 7 * n); break;
    case Unit::kMonth: lo = midnight(t.y, t.m - n, t.d); break;
    case Unit::kYear: lo = midnight(t.y - n, t.m, t.d); break;
    case Unit::kNone: return std::nullopt;
  }
  return Period{lo, midnight(t.y, t.m, t.d + 1)};
}

// The day / week / month / year N units before today.
std::optional<Period> ago_period(int n, Unit u, const Today& t) {
  switch (u) {
    case Unit::kDay: return day_period(t.y, t.m, t.d - n);
    case Unit::kWeek: {
      const int monday = t.d - t.wd - 7 * n;
      return Period{midnight(t.y, t.m, monday), midnight(t.y, t.m, monday + 7)};
    }
    case Unit::kMonth: return month_period(t.y, t.m - n);
    case Unit::kYear: return year_period(t.y - n);
    case Unit::kNone: break;
  }
  return std::nullopt;
}

struct Match {
  size_t n = 0;
  Period p;
};

class Matcher {
 public:
  Matcher(std::span<const std::string_view> raw, const DateOptions& o, bool bare) : o_(o), t_(today_of(o.now)), bare_(bare) {
    words_.reserve(raw.size());
    for (std::string_view w : raw) words_.push_back(norm_word(w));
  }

  std::optional<Match> at(size_t i) const {
    if (i >= words_.size()) return std::nullopt;
    if (words_[i] == "since") {
      auto m = simple(i + 1);
      if (!m) return std::nullopt;
      return Match{m->n + 1, {m->p.lo, kOpenTo}};
    }
    if (words_[i] == "between") {
      auto a = simple(i + 1);
      if (!a) return std::nullopt;
      const size_t and_at = i + 1 + a->n;
      if (and_at >= words_.size() || words_[and_at] != "and") return std::nullopt;
      auto b = simple(and_at + 1);
      if (!b || b->p.hi <= a->p.lo) return std::nullopt;
      return Match{a->n + b->n + 2, {a->p.lo, b->p.hi}};
    }
    return simple(i);
  }

  size_t size() const { return words_.size(); }

 private:
  const std::string& w(size_t i) const {
    static const std::string kEmpty;
    return i < words_.size() ? words_[i] : kEmpty;
  }

  // Every production but since / between; the longest wins.
  std::optional<Match> simple(size_t i) const {
    std::optional<Match> best;
    auto offer = [&](size_t n, std::optional<Period> p) {
      if (p && p->lo < p->hi && (!best || n > best->n)) best = Match{n, *p};
    };
    const std::string& a = w(i);
    const std::string& b = w(i + 1);
    const std::string& c = w(i + 2);
    if (a.empty()) return std::nullopt;

    if (a == "today") offer(1, day_period(t_.y, t_.m, t_.d));
    if (a == "yesterday") offer(1, day_period(t_.y, t_.m, t_.d - 1));
    if (a == "this" || a == "last") {
      const int back = a == "last" ? 1 : 0;
      if (b == "week") {
        const int monday = t_.d - t_.wd - 7 * back;
        offer(2, Period{midnight(t_.y, t_.m, monday), midnight(t_.y, t_.m, monday + 7)});
      }
      if (b == "month") offer(2, month_period(t_.y, t_.m - back));
      if (b == "year") offer(2, year_period(t_.y - back));
    }
    if (a == "last") {
      // The most recent month of that name before the current month.
      if (int m = month_index(b, true, o_); m != 0) offer(2, month_period(m < t_.m ? t_.y : t_.y - 1, m));
      // The most recent such weekday before today.
      if (int wd = weekday_index(b, true, o_); wd != 0) {
        int delta = (t_.wd - (wd - 1) + 7) % 7;
        if (delta == 0) delta = 7;
        offer(2, day_period(t_.y, t_.m, t_.d - delta));
      }
    }
    if (auto n = parse_int(a, 1, 5); n && *n <= kMaxAgo && c == "ago") {
      if (Unit u = unit_word(b); u != Unit::kNone) offer(3, ago_period(*n, u, t_));
    }
    if (a.size() == 2 && a[0] == 'q' && a[1] >= '1' && a[1] <= '4') {
      const int q = a[1] - '0';
      if (auto y = parse_year(b)) {
        offer(2, Period{midnight(*y, 3 * q - 2, 1), midnight(*y, 3 * q + 1, 1)});
      } else {
        const int cur_q = (t_.m - 1) / 3 + 1;
        const int y0 = q > cur_q ? t_.y - 1 : t_.y;
        offer(1, Period{midnight(y0, 3 * q - 2, 1), midnight(y0, 3 * q + 1, 1)});
      }
    }
    offer(1, numeric_token(a, o_));

    // Day month [year] / month day [year] / month year / month / weekday.
    if (auto d = parse_day(a)) {
      if (int m = month_index(b, true, o_); m != 0) {
        if (auto y = parse_year(c)) {
          offer(3, checked_day(*y, m, *d));
        } else {
          offer(2, yearless_day(m, *d, t_));
        }
      }
    }
    if (int m = month_index(a, true, o_); m != 0) {
      if (auto y = parse_year(b)) offer(2, month_period(*y, m));
      if (auto d = parse_day(b); d && b.size() <= 4) {
        if (auto y = parse_year(c)) {
          offer(3, checked_day(*y, m, *d));
        } else {
          offer(2, yearless_day(m, *d, t_));
        }
      }
      // A lone month: bare text needs the full name and never takes `may`.
      if (int full = month_index(a, false, o_); !bare_ || (full != 0 && a != "may")) {
        offer(1, month_period(m > t_.m ? t_.y - 1 : t_.y, m));
      }
    }
    if (int wd = weekday_index(a, !bare_, o_); wd != 0) {
      const int delta = (t_.wd - (wd - 1) + 7) % 7;
      offer(1, day_period(t_.y, t_.m, t_.d - delta));
    }
    if (!bare_) {
      if (auto y = parse_year(a)) offer(1, year_period(*y));
      if (auto dur = duration_token(a)) offer(1, duration_period(dur->first, dur->second, t_));
      if (auto n = parse_int(a, 1, 5); n && *n >= 1 && *n <= kMaxAgo) {
        if (Unit u = unit_word(b); u != Unit::kNone) offer(2, duration_period(*n, u, t_));
      }
    }
    return best;
  }

  const DateOptions& o_;
  Today t_;
  bool bare_;
  std::vector<std::string> words_;
};

std::vector<std::string_view> split_words(std::string_view s) {
  std::vector<std::string_view> out;
  size_t i = 0;
  while (i < s.size()) {
    while (i < s.size() && (s[i] == ' ' || s[i] == '\t')) ++i;
    const size_t start = i;
    while (i < s.size() && s[i] != ' ' && s[i] != '\t') ++i;
    if (i > start) out.push_back(s.substr(start, i - start));
  }
  return out;
}

std::optional<Period> single_value(std::string_view value, const DateOptions& o) {
  const auto words = split_words(value);
  if (words.empty() || words.size() > 16) return std::nullopt;
  Matcher m(words, o, false);
  auto r = m.at(0);
  if (!r || r->n != words.size()) return std::nullopt;
  return r->p;
}

struct LocalDay {
  int y, m, d;
};

LocalDay local_day(int64_t t) {
  const time_t tt = static_cast<time_t>(t);
  std::tm tmv{};
  localtime_r(&tt, &tmv);
  return {tmv.tm_year + 1900, tmv.tm_mon + 1, tmv.tm_mday};
}

std::string fmt_day(const LocalDay& d) {
  return std::to_string(d.d) + " " + std::string(kMonthAbbr[static_cast<size_t>(d.m - 1)]) + " " + std::to_string(d.y);
}

}  // namespace

std::optional<Period> parse_date_value(std::string_view value, const DateOptions& options) {
  if (value.size() > 256) return std::nullopt;
  if (const size_t dots = value.find(".."); dots != std::string_view::npos) {
    auto a = single_value(value.substr(0, dots), options);
    auto b = single_value(value.substr(dots + 2), options);
    if (!a || !b || b->hi <= a->lo || a->lo == kOpenFrom || b->hi == kOpenTo) return std::nullopt;
    return Period{a->lo, b->hi};
  }
  return single_value(value, options);
}

size_t match_date_phrase(std::span<const std::string_view> words, const DateOptions& options, Period* out) {
  // A word holding the range operator ends the phrase: the chip prints as `date:"<source text>"`, and that value
  // would re-parse as a `..` range ("march 2.." would otherwise read as March 2, its dots stripped like "July 5.").
  size_t n = std::min<size_t>(words.size(), 16);
  for (size_t i = 0; i < n; ++i) {
    if (words[i].find("..") != std::string_view::npos) n = i;
  }
  if (n == 0) return 0;
  Matcher m(words.first(n), options, true);
  auto r = m.at(0);
  if (!r) return 0;
  *out = r->p;
  return r->n;
}

Period apply_date_op(DateOp op, const Period& p) {
  switch (op) {
    case DateOp::kOn: return p;
    case DateOp::kBefore: return {kOpenFrom, p.lo};
    case DateOp::kAfter: return {p.lo, kOpenTo};
  }
  return p;
}

std::string describe_range(int64_t from, int64_t to) {
  if (from == kOpenFrom && to == kOpenTo) return "any date";
  if (from == kOpenFrom) return "before " + fmt_day(local_day(to));
  if (to == kOpenTo) return "since " + fmt_day(local_day(from));
  const LocalDay a = local_day(from);
  if (from == midnight(a.y, a.m, a.d)) {
    if (a.m == 1 && a.d == 1 && to == midnight(a.y + 1, 1, 1)) return std::to_string(a.y);
    if (a.d == 1 && to == midnight(a.y, a.m + 1, 1)) {
      return std::string(kMonthTitle[static_cast<size_t>(a.m - 1)]) + " " + std::to_string(a.y);
    }
    if (to == midnight(a.y, a.m, a.d + 1)) return fmt_day(a);
  }
  return fmt_day(a) + " \u2013 " + fmt_day(local_day(to - 1));
}

}  // namespace mailsearch
