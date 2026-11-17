// Query language: keyword and alias vectors, operators, dates against a fixed "now",
// canonical round trip, and the never-fails property on random input.
#include <gtest/gtest.h>

#include <algorithm>
#include <cstdlib>
#include <ctime>
#include <random>
#include <string>
#include <vector>

#include "mailsearch/query_parser.h"

namespace s = mailsearch;

namespace {

// 2026-11-14T12:00:00Z, a Saturday; dates in a fixed zone (POSIX rule, no tzdata needed).
constexpr int64_t kNow = 1794657600;

uint32_t root_of(const s::Query& q) { return q.root.value_or(0); }

int64_t local(int y, int m, int d) {
  std::tm t{};
  t.tm_year = y - 1900;
  t.tm_mon = m - 1;
  t.tm_mday = d;
  t.tm_isdst = -1;
  return static_cast<int64_t>(std::mktime(&t));
}

class Query : public ::testing::Test {
 protected:
  void SetUp() override {
    setenv("TZ", "CET-1CEST,M3.5.0,M10.5.0/3", 1);
    tzset();
    opts.dates.now = kNow;
  }

  s::ParseResult parse(const std::string& text) { return s::parse_query(text, opts); }
  std::string canon(const std::string& text) { return s::canonical_text(parse(text).query, opts); }

  // The root's children (or the root alone).
  std::vector<const s::Node*> top(const s::ParseResult& r) {
    std::vector<const s::Node*> out;
    if (r.query.empty()) return out;
    const s::Node& root = r.query.node(root_of(r.query));
    if (root.kind == s::NodeKind::kAnd) {
      for (uint32_t c : root.children) out.push_back(&r.query.node(c));
    } else {
      out.push_back(&root);
    }
    return out;
  }

  const s::Node& only(const s::ParseResult& r) {
    auto t = top(r);
    EXPECT_EQ(t.size(), 1u);
    return *t.at(0);
  }

  bool has_hint(const s::ParseResult& r, s::HintKind k) {
    for (const auto& h : r.hints) {
      if (h.kind == k) return true;
    }
    return false;
  }

  void expect_round_trip(const std::string& text) {
    const s::ParseResult a = parse(text);
    const std::string c1 = s::canonical_text(a.query, opts);
    const s::ParseResult b = parse(c1);
    EXPECT_TRUE(s::same_query(a.query, b.query)) << "input: " << text << "\ncanonical: " << c1 << "\nagain: "
                                                 << s::canonical_text(b.query, opts);
    EXPECT_EQ(c1, s::canonical_text(b.query, opts)) << text;
  }

  s::ParseOptions opts;
};

TEST_F(Query, BareTermsAreAndedTextTerms) {
  auto r = parse("invoice acme");
  auto t = top(r);
  ASSERT_EQ(t.size(), 2u);
  EXPECT_EQ(t[0]->kind, s::NodeKind::kTerm);
  EXPECT_EQ(t[0]->text, "invoice");
  EXPECT_EQ(t[0]->field, s::Field::kText);
  EXPECT_EQ(t[0]->mode, s::TermMode::kDefault);
  EXPECT_EQ(canon("invoice   acme"), "invoice acme");
  EXPECT_TRUE(parse("").query.empty());
  EXPECT_TRUE(parse("   ").query.empty());
}

TEST_F(Query, TextKeywordsAndAliases) {
  struct V {
    const char* in;
    s::Field field;
    const char* canonical;
  };
  const V vectors[] = {
      {"text:a", s::Field::kText, "a"},
      {"content:a", s::Field::kContent, "content:a"},
      {"Subject:a", s::Field::kSubject, "subject:a"},
      {"body:a", s::Field::kBody, "body:a"},
      {"from:john", s::Field::kFrom, "from:john"},
      {"to:a", s::Field::kTo, "to:a"},
      {"cc:a", s::Field::kCc, "cc:a"},
      {"bcc:a", s::Field::kBcc, "bcc:a"},
      {"recipient:a", s::Field::kRecipient, "recipient:a"},
      {"email:a", s::Field::kEmail, "email:a"},
      {"with:a", s::Field::kEmail, "email:a"},
      {"participant:a", s::Field::kEmail, "email:a"},
      {"attachmentName:q3", s::Field::kAttachmentName, "attachmentName:q3"},
      {"filename:q3", s::Field::kAttachmentName, "attachmentName:q3"},
      {"attachmentContent:q3", s::Field::kAttachmentContent, "attachmentContent:q3"},
      {"attached:q3", s::Field::kAttachmentContent, "attachmentContent:q3"},
  };
  for (const V& v : vectors) {
    auto r = parse(v.in);
    const s::Node& n = only(r);
    EXPECT_EQ(n.kind, s::NodeKind::kTerm) << v.in;
    EXPECT_EQ(n.field, v.field) << v.in;
    EXPECT_EQ(s::canonical_text(r.query, opts), v.canonical) << v.in;
    EXPECT_TRUE(r.hints.empty()) << v.in;
  }
}

TEST_F(Query, FilterKeywordsAndAliases) {
  struct V {
    const char* in;
    s::FilterKey key;
    const char* value;
    const char* canonical;
  };
  const V vectors[] = {
      {"attachment:yes", s::FilterKey::kAttachment, "yes", "attachment:yes"},
      {"has:attachment", s::FilterKey::kAttachment, "yes", "attachment:yes"},
      {"has:PDF", s::FilterKey::kHas, "pdf", "has:pdf"},
      {"has:spreadsheet", s::FilterKey::kHas, "spreadsheet", "has:spreadsheet"},
      {"read:no", s::FilterKey::kRead, "no", "read:no"},
      {"is:unread", s::FilterKey::kRead, "no", "read:no"},
      {"is:read", s::FilterKey::kRead, "yes", "read:yes"},
      {"flag:yes", s::FilterKey::kFlagged, "yes", "flag:yes"},
      {"is:flagged", s::FilterKey::kFlagged, "yes", "flag:yes"},
      {"is:starred", s::FilterKey::kFlagged, "yes", "flag:yes"},
      {"is:sent", s::FilterKey::kIs, "sent", "is:sent"},
      {"is:replied", s::FilterKey::kIs, "replied", "is:replied"},
      {"is:encrypted", s::FilterKey::kEncrypted, "yes", "encrypted:yes"},
      {"signed:no", s::FilterKey::kSigned, "no", "signed:no"},
      {"importance:2", s::FilterKey::kImportance, "high", "importance:high"},
      {"tag:Invoices", s::FilterKey::kTag, "Invoices", "tag:Invoices"},
      {"label:\"To Do\"", s::FilterKey::kTag, "To Do", "tag:\"To Do\""},
      {"in:inbox", s::FilterKey::kFolder, "inbox", "folder:inbox"},
      {"in:spam", s::FilterKey::kFolder, "junk", "folder:junk"},
      {"folder:Projects/Acme", s::FilterKey::kFolder, "Projects/Acme", "folder:Projects/Acme"},
      {"in:anywhere", s::FilterKey::kFolder, "anywhere", "folder:anywhere"},
      {"account:work", s::FilterKey::kAccount, "work", "account:work"},
      {"larger:20kB", s::FilterKey::kLarger, "20480", "larger:20480"},
      {"size:1.5MB", s::FilterKey::kLarger, "1572864", "larger:1572864"},
      {"smaller:30MB", s::FilterKey::kSmaller, "31457280", "smaller:31457280"},
      {"msgid:<abc@example.com>", s::FilterKey::kMsgId, "abc@example.com", "msgid:abc@example.com"},
      {"rfc822msgid:abc@example.com", s::FilterKey::kMsgId, "abc@example.com", "msgid:abc@example.com"},
      {"reply:alice", s::FilterKey::kReply, "alice", "reply:alice"},
      {"deliveredto:me@work.com", s::FilterKey::kDeliveredTo, "me@work.com", "deliveredto:me@work.com"},
      {"list:dev.lists.example.org", s::FilterKey::kList, "dev.lists.example.org", "list:dev.lists.example.org"},
  };
  for (const V& v : vectors) {
    auto r = parse(v.in);
    const s::Node& n = only(r);
    EXPECT_EQ(n.kind, s::NodeKind::kFilter) << v.in;
    EXPECT_EQ(n.key, v.key) << v.in;
    EXPECT_EQ(n.value, v.value) << v.in;
    EXPECT_EQ(s::canonical_text(r.query, opts), v.canonical) << v.in;
  }
}

TEST_F(Query, UnsupportedKeywordsShowNotAvailableChips) {
  for (const char* in : {"notes:x", "category:red", "header:X-Foo", "is:muted", "in:snoozed", "is:important"}) {
    auto r = parse(in);
    const s::Node& n = only(r);
    EXPECT_EQ(n.kind, s::NodeKind::kFilter) << in;
    EXPECT_EQ(n.key, s::FilterKey::kUnavailable) << in;
    EXPECT_EQ(n.value, in) << in;
    EXPECT_TRUE(has_hint(r, s::HintKind::kNotAvailable)) << in;
    EXPECT_NE(s::describe_node(r.query, root_of(r.query)).find("not available"), std::string::npos) << in;
    expect_round_trip(in);
  }
}

TEST_F(Query, SpaceAfterColonIsLiteral) {
  auto r = parse("from: alice");
  auto t = top(r);
  ASSERT_EQ(t.size(), 2u);
  EXPECT_EQ(t[0]->kind, s::NodeKind::kTerm);
  EXPECT_EQ(t[0]->text, "from:");
  EXPECT_EQ(t[0]->field, s::Field::kText);
  EXPECT_EQ(t[1]->text, "alice");
  expect_round_trip("from: alice");
  expect_round_trip("a from:");
}

TEST_F(Query, UnknownSyntaxIsTreatedAsText) {
  auto r = parse("foo:bar");
  EXPECT_EQ(only(r).text, "foo:bar");
  EXPECT_TRUE(has_hint(r, s::HintKind::kTreatedAsText));
  r = parse("before:someday");
  EXPECT_EQ(only(r).kind, s::NodeKind::kTerm);
  EXPECT_EQ(only(r).text, "before:someday");
  EXPECT_TRUE(has_hint(r, s::HintKind::kTreatedAsText));
  expect_round_trip("before:someday");
  r = parse("is:fancy larger:huge");
  EXPECT_EQ(top(r).size(), 2u);
  EXPECT_EQ(top(r)[0]->kind, s::NodeKind::kTerm);
  // URLs stay plain text without a hint.
  r = parse("https://example.com/x");
  EXPECT_EQ(only(r).text, "https://example.com/x");
  EXPECT_FALSE(has_hint(r, s::HintKind::kTreatedAsText));
  // Infix wildcards outside header fields.
  r = parse("j*smith");
  EXPECT_EQ(only(r).mode, s::TermMode::kDefault);
  EXPECT_TRUE(has_hint(r, s::HintKind::kTreatedAsText));
  expect_round_trip("j*smith");
}

TEST_F(Query, Operators) {
  auto r = parse("a OR b");
  EXPECT_EQ(only(r).kind, s::NodeKind::kOr);
  EXPECT_EQ(canon("a OR b"), "a OR b");
  EXPECT_EQ(canon("{a b}"), "a OR b");
  EXPECT_EQ(canon("x {a b}"), "x a OR b");
  EXPECT_TRUE(s::same_query(parse("x {a b}").query, parse("x a OR b").query));

  r = parse("from:a,b");
  const s::Node& orn = only(r);
  ASSERT_EQ(orn.kind, s::NodeKind::kOr);
  ASSERT_EQ(orn.children.size(), 2u);
  EXPECT_EQ(r.query.node(orn.children[1]).field, s::Field::kFrom);
  EXPECT_EQ(canon("from:a,b"), "from:a OR from:b");

  EXPECT_TRUE(s::same_query(parse("-from:x").query, parse("from:-x").query));
  r = parse("-a");
  EXPECT_EQ(only(r).kind, s::NodeKind::kNot);
  EXPECT_EQ(canon("-(a b)"), "-(a b)");
  EXPECT_EQ(canon("-{a b}"), "-(a OR b)");

  r = parse("(a b) c");
  ASSERT_EQ(top(r).size(), 2u);
  EXPECT_EQ(top(r)[0]->kind, s::NodeKind::kAnd);
  EXPECT_EQ(canon("(a b) c"), "(a b) c");
  EXPECT_EQ(canon("(a)"), "a");

  r = parse("\"quarterly  report\"");
  EXPECT_EQ(only(r).mode, s::TermMode::kPhrase);
  EXPECT_EQ(only(r).text, "quarterly report");

  EXPECT_EQ(only(parse("+word")).mode, s::TermMode::kExact);
  EXPECT_EQ(only(parse("inv*")).mode, s::TermMode::kPrefix);
  EXPECT_EQ(only(parse("inv*")).text, "inv");

  r = parse("budget AROUND 5 meeting");
  EXPECT_EQ(only(r).mode, s::TermMode::kNear);
  EXPECT_EQ(only(r).near, 5u);
  EXPECT_EQ(only(r).text, "budget meeting");
  EXPECT_EQ(canon("budget AROUND 5 meeting"), "budget AROUND 5 meeting");

  r = parse("from:*green.com");
  EXPECT_EQ(only(r).mode, s::TermMode::kWildcard);
  EXPECT_EQ(only(r).text, "*green.com");
  EXPECT_EQ(canon("from:*green.com"), "from:*green.com");

  r = parse("? from:x");
  EXPECT_TRUE(r.keyword_list);
  EXPECT_EQ(only(r).field, s::Field::kFrom);

  EXPECT_EQ(only(parse("a AND")).text, "a");
  EXPECT_EQ(top(parse("a AND b")).size(), 2u);
}

TEST_F(Query, PersonChips) {
  auto r = parse("from:<John@Contoso.com>");
  const s::Node& n = only(r);
  ASSERT_EQ(n.kind, s::NodeKind::kPerson);
  ASSERT_EQ(n.addresses.size(), 1u);
  EXPECT_EQ(n.addresses[0], "john@contoso.com");
  EXPECT_EQ(n.roles, s::kRoleFrom);
  EXPECT_EQ(canon("from:<a@x.com>,<b@y.com>"), "from:<a@x.com>,<b@y.com>");
  EXPECT_EQ(only(parse("email:<a@x.com>")).roles, s::kRoleAny);
  r = parse("from:me");
  EXPECT_TRUE(only(r).me);
  EXPECT_EQ(canon("to:ME"), "to:me");
  EXPECT_EQ(only(parse("from:\\me")).kind, s::NodeKind::kTerm);
  expect_round_trip("from:\\me");
  expect_round_trip("subject:me");
}

TEST_F(Query, AbsoluteDates) {
  struct V {
    const char* in;
    int64_t from, to;
  };
  const V vectors[] = {
      {"date:2024-07-05", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:2024/07/05", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:2024-07", local(2024, 7, 1), local(2024, 8, 1)},
      {"date:2024", local(2024, 1, 1), local(2025, 1, 1)},
      {"date:\"5 July 2024\"", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:\"July 5 2024\"", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:\"July 5, 2024\"", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:\"5 Jul 2024\"", local(2024, 7, 5), local(2024, 7, 6)},
      {"date:07/05/2024", local(2024, 5, 7), local(2024, 5, 8)},  // LC_TIME day first
      {"date:07.05.2024", local(2024, 5, 7), local(2024, 5, 8)},
      {"date:07-05-2024", local(2024, 5, 7), local(2024, 5, 8)},
      {"date:13/05/2024", local(2024, 5, 13), local(2024, 5, 14)},
      {"date:05/13/2024", local(2024, 5, 13), local(2024, 5, 14)},  // 13 cannot be a month
      {"date:2024-03-01..2024-03-15", local(2024, 3, 1), local(2024, 3, 16)},
      {"before:2024-07-05", s::kOpenFrom, local(2024, 7, 5)},  // before D excludes D
      {"after:2024-07-05", local(2024, 7, 5), s::kOpenTo},     // after D includes D
      {"older:2024-07", s::kOpenFrom, local(2024, 7, 1)},
      {"newer:2024", local(2024, 1, 1), s::kOpenTo},
      {"received-before:2024-07-05", s::kOpenFrom, local(2024, 7, 5)},
  };
  for (const V& v : vectors) {
    auto r = parse(v.in);
    const s::Node& n = only(r);
    ASSERT_EQ(n.kind, s::NodeKind::kDateRange) << v.in;
    EXPECT_EQ(n.from, v.from) << v.in;
    EXPECT_EQ(n.to, v.to) << v.in;
    expect_round_trip(v.in);
  }
  EXPECT_EQ(only(parse("before:2024-07-05")).date_field, s::DateField::kSent);
  EXPECT_EQ(only(parse("received-after:2024-07-05")).date_field, s::DateField::kReceived);
  opts.dates.day_first = false;  // en_US
  EXPECT_EQ(only(parse("date:07/05/2024")).from, local(2024, 7, 5));
  // Invalid calendar dates are not dates.
  EXPECT_EQ(only(parse("date:2023-02-29")).kind, s::NodeKind::kTerm);
  EXPECT_EQ(only(parse("date:2024-13")).kind, s::NodeKind::kTerm);
}

TEST_F(Query, NaturalDates) {
  struct V {
    const char* in;
    int64_t from, to;
  };
  const V vectors[] = {
      {"today", local(2026, 11, 14), local(2026, 11, 15)},
      {"yesterday", local(2026, 11, 13), local(2026, 11, 14)},
      {"this week", local(2026, 11, 9), local(2026, 11, 16)},
      {"last week", local(2026, 11, 2), local(2026, 11, 9)},
      {"this month", local(2026, 11, 1), local(2026, 12, 1)},
      {"last month", local(2026, 10, 1), local(2026, 11, 1)},
      {"this year", local(2026, 1, 1), local(2027, 1, 1)},
      {"last year", local(2025, 1, 1), local(2026, 1, 1)},
      {"last march", local(2026, 3, 1), local(2026, 4, 1)},        // before the current month
      {"last november", local(2025, 11, 1), local(2025, 12, 1)},   // the current month's name: a year back
      {"last december", local(2025, 12, 1), local(2026, 1, 1)},
      {"march 2024", local(2024, 3, 1), local(2024, 4, 1)},
      {"december", local(2025, 12, 1), local(2026, 1, 1)},          // the most recent December
      {"may 2024", local(2024, 5, 1), local(2024, 6, 1)},
      {"monday", local(2026, 11, 9), local(2026, 11, 10)},
      {"saturday", local(2026, 11, 14), local(2026, 11, 15)},
      {"last saturday", local(2026, 11, 7), local(2026, 11, 8)},
      {"3 days ago", local(2026, 11, 11), local(2026, 11, 12)},
      {"2 weeks ago", local(2026, 10, 26), local(2026, 11, 2)},
      {"1 month ago", local(2026, 10, 1), local(2026, 11, 1)},
      {"2 years ago", local(2024, 1, 1), local(2025, 1, 1)},
      {"since yesterday", local(2026, 11, 13), s::kOpenTo},
      {"since 2024-03-01", local(2024, 3, 1), s::kOpenTo},
      {"between 2024-03-01 and 2024-03-15", local(2024, 3, 1), local(2024, 3, 16)},
      {"between 1 march 2024 and 15 march 2024", local(2024, 3, 1), local(2024, 3, 16)},
      {"q1 2024", local(2024, 1, 1), local(2024, 4, 1)},
      {"q4", local(2026, 10, 1), local(2027, 1, 1)},
      {"q1", local(2026, 1, 1), local(2026, 4, 1)},
      {"5 july", local(2026, 7, 5), local(2026, 7, 6)},
      {"2024-07-05", local(2024, 7, 5), local(2024, 7, 6)},
      {"07/05/2024", local(2024, 5, 7), local(2024, 5, 8)},
  };
  for (const V& v : vectors) {
    auto r = parse(v.in);
    const s::Node& n = only(r);
    ASSERT_EQ(n.kind, s::NodeKind::kDateRange) << v.in;
    EXPECT_EQ(n.from, v.from) << v.in;
    EXPECT_EQ(n.to, v.to) << v.in;
    EXPECT_EQ(n.source_text, v.in) << v.in;
    EXPECT_EQ(n.date_field, s::DateField::kReceived) << v.in;
    expect_round_trip(v.in);
  }
  // In March 2026, `last march` is March 2025.
  opts.dates.now = local(2026, 3, 10) + 12 * 3600;
  EXPECT_EQ(only(parse("last march")).from, local(2025, 3, 1));
  EXPECT_EQ(only(parse("date:\"last march\"")).from, local(2025, 3, 1));
}

TEST_F(Query, RelativeKeywordValues) {
  auto n = only(parse("within:2d"));
  EXPECT_EQ(n.kind, s::NodeKind::kDateRange);
  EXPECT_EQ(n.from, local(2026, 11, 12));
  EXPECT_EQ(n.to, s::kOpenTo);
  n = only(parse("newer_than:1Week"));
  EXPECT_EQ(n.from, local(2026, 11, 7));
  n = only(parse("received-within:1Month"));
  EXPECT_EQ(n.from, local(2026, 10, 14));
  n = only(parse("older_than:1y"));
  EXPECT_EQ(n.from, s::kOpenFrom);
  EXPECT_EQ(n.to, local(2025, 11, 14));
  EXPECT_EQ(canon("within:2d"), "received-after:2d");
  EXPECT_EQ(canon("older_than:1y"), "received-before:1y");
  expect_round_trip("within:2d");
}

TEST_F(Query, DatePhrasesInBareTextBecomeChips) {
  auto r = parse("invoice last march");
  auto t = top(r);
  ASSERT_EQ(t.size(), 2u);
  EXPECT_EQ(t[0]->text, "invoice");
  EXPECT_EQ(t[1]->kind, s::NodeKind::kDateRange);
  EXPECT_EQ(t[1]->source_text, "last march");
  EXPECT_EQ(t[1]->from, local(2026, 3, 1));
  EXPECT_EQ(s::describe_node(r.query, r.query.node(root_of(r.query)).children[1]), "March 2026");
  EXPECT_EQ(canon("invoice last march"), "invoice date:\"last march\"");
  expect_round_trip("invoice last march");

  // Ordinary words stay words: lone `may`, bare years, durations, seasons, quoted and grouped text.
  for (const char* in : {"may", "report 2024", "2d", "summer 2024", "\"last march\"", "(last march)", "-today", "{today x}"}) {
    auto rr = parse(in);
    for (const auto& n : rr.query.nodes) EXPECT_NE(n.kind, s::NodeKind::kDateRange) << in;
    expect_round_trip(in);
  }
  // A word holding the range operator `..` ends the phrase: `date:"march 2.."` would re-parse as a range
  // (fuzz finding: "march 2.." became March 2 and its canonical text a term).
  for (const char* in : {"march 2..", "march 2...", "2.. march", "march..", "today.. x"}) {
    auto rr = parse(in);
    for (const auto& n : rr.query.nodes) EXPECT_TRUE(n.kind != s::NodeKind::kDateRange || n.source_text.find("..") == std::string::npos) << in;
    expect_round_trip(in);
  }
  r = parse("march 2..");
  t = top(r);
  ASSERT_EQ(t.size(), 2u);
  EXPECT_EQ(t[0]->source_text, "march");
  EXPECT_EQ(t[1]->text, "2..");

  // Escaped date words are text, and print escaped.
  r = parse("\\today report");
  EXPECT_EQ(top(r)[0]->kind, s::NodeKind::kTerm);
  expect_round_trip("\\today report");
}

TEST_F(Query, RemovingADateChipLeavesAHint) {
  auto r = parse("invoice last march");
  const uint32_t date = r.query.node(root_of(r.query)).children[1];
  const s::Query q = s::remove_chip(r.query, date);
  ASSERT_TRUE(q.hint.has_value());
  const s::DateHint hint = q.hint.value_or(s::DateHint{});
  EXPECT_EQ(hint.from, local(2026, 3, 1));
  EXPECT_EQ(hint.to, local(2026, 4, 1));
  EXPECT_EQ(s::canonical_text(q, opts), "invoice");
  // Removing another chip drops it and collapses the group.
  r = parse("a from:b -is:unread");
  const s::Node& root = r.query.node(root_of(r.query));
  const s::Query q2 = s::remove_chip(r.query, root.children[2]);
  EXPECT_EQ(s::canonical_text(q2, opts), "a from:b");
  EXPECT_FALSE(q2.hint.has_value());
}

TEST_F(Query, ChipLabels) {
  EXPECT_EQ(s::describe_node(parse("before:2024-07-05").query, 0), "sent before 5 Jul 2024");
  EXPECT_EQ(s::describe_range(local(2024, 3, 1), s::kOpenTo), "since 1 Mar 2024");
  EXPECT_EQ(s::describe_range(local(2024, 1, 1), local(2025, 1, 1)), "2024");
  EXPECT_EQ(s::describe_range(local(2024, 7, 5), local(2024, 7, 6)), "5 Jul 2024");
  EXPECT_EQ(s::describe_range(local(2026, 3, 2), local(2026, 3, 9)), "2 Mar 2026 \u2013 8 Mar 2026");
  auto r = parse("is:unread");
  EXPECT_EQ(s::describe_node(r.query, root_of(r.query)), "is unread");
}

TEST_F(Query, KeywordCompletions) {
  auto c = s::keyword_completions("att");
  EXPECT_NE(std::find(c.begin(), c.end(), "attachmentName:"), c.end());
  EXPECT_NE(std::find(c.begin(), c.end(), "attached:"), c.end());
  EXPECT_TRUE(s::is_keyword("received-within"));
  EXPECT_FALSE(s::is_keyword("foo"));
}

TEST_F(Query, CanonicalRoundTrip) {
  const char* inputs[] = {
      "invoice", "from:john subject:report", "a OR b c", "(a OR b) (c OR d)", "-(a b) c", "--a",
      "from:a,b,<c@d.com>", "{from:a to:b} is:unread", "\"exact phrase\" +word pre*", R"(subject:"a \"quoted\" b")",
      "tag:\"To Do\",Urgent", "folder:\"My Folder\" -in:trash", "OR", "a OR", "AND", "AROUND", "a AROUND x", "x AROUND 3 y z",
      "a\\ b", "\\-dash", "\\(paren\\)", "c\\:d", "from\\:x", "subject:x\\,y", "100%", "café naïve 会議", "a:b:c",
      "before:\"5 July 2024\" after:2024-01-01", "date:2024-03-01..2024-03-15", "older_than:6m newer_than:2d",
      "?", "? hello", "\"unterminated phrase", "(unclosed group", "stray ) close } brace", "\"\"", "()", "{}",
      "- + -- ++", "-\"phrase\"", "+\"phrase\"", "subject:+exact", "subject:pre*", "subject:j*smith", "body:*x",
      "from:", "from:,", "from:a,", "is:", "notes:a,b", "IN:Snoozed", "has:ATTACHMENT", "importance:normal",
      "msgid:<a@b>", "larger:0", "smaller:5g", "between yesterday and today", "since last week", "q2 2025 report",
      "invoice 5 july march 2024", "07.05.2024 x", "march, report", "monday OR tuesday", "a OR last march",
      "{a OR b c}", "{a OR b OR c {d OR e}}", "-{x OR y}",
      "((((((((((((((( tag:-x tag:y)))))))))))))))",
      R"(from:<a\\b@c.d>)", R"(from:jo,<a\,b@c.d>)", R"(to:<a\)b@c.d> x)", R"(cc:<a\ b@c.d>)",
  };
  for (const char* in : inputs) expect_round_trip(in);
}

// A value list is an Or that canonical text prints as `(a OR b)` inside an Or or after `-`, so it follows the depth rule of
// `(`: at kMaxDepth it keeps its first value (fuzz finding crash-d888eb1a…: 15 nested groups, then `from:j,c` beside an OR,
// printed a 16th group that re-parsed truncated).
TEST_F(Query, ValueListFollowsTheDepthRule) {
  std::string deep;  // the text after it is at depth kMaxDepth - 1
  for (size_t i = 0; i + 1 < s::kMaxDepth; ++i) deep += "(a ";
  const std::string one_up = deep.substr(3);
  const std::string two_up = deep.substr(6);
  for (const char* tail : {"y OR from:a,b", "y OR tag:x,y", "y OR before:2024,2025", "y OR -from:a,b", "y OR from:-a,b", "from:a,b",
                           "y OR from:<a@b.c>,<d@e.f>", "y OR from:a,<d@e.f>,me"}) {
    for (const std::string& lead : {deep, one_up, two_up}) {
      expect_round_trip("x OR " + lead + tail);
      expect_round_trip("x " + lead + tail);
      expect_round_trip("x OR {a " + lead.substr(3) + tail);
    }
  }
  const auto from_terms = [](const s::ParseResult& r) {
    size_t n = 0;
    for (const auto& node : r.query.nodes) n += (node.kind == s::NodeKind::kTerm && node.field == s::Field::kFrom) ? 1 : 0;
    return n;
  };
  auto r = parse("x OR " + deep + "y OR from:a,b");
  EXPECT_EQ(from_terms(r), 1u);
  EXPECT_TRUE(has_hint(r, s::HintKind::kTruncated));
  r = parse("x OR " + one_up + "y OR from:a,b");
  EXPECT_EQ(from_terms(r), 2u);
  EXPECT_FALSE(has_hint(r, s::HintKind::kTruncated));
  // Under `-` the list sits one level deeper.
  for (const char* tail : {"y OR -from:a,b", "y OR from:-a,b"}) {
    r = parse("x OR " + one_up + tail);
    EXPECT_EQ(from_terms(r), 1u) << tail;
    EXPECT_TRUE(has_hint(r, s::HintKind::kTruncated)) << tail;
    r = parse("x OR " + two_up + tail);
    EXPECT_EQ(from_terms(r), 2u) << tail;
  }
  // One Person of several addresses is not a list.
  r = parse("x OR " + deep + "y OR from:<a@b.c>,<d@e.f>");
  EXPECT_FALSE(has_hint(r, s::HintKind::kTruncated));
  // The minimised fuzz input (cpp/fuzz/corpus/query/regress-value-list-depth-min, mode byte 0).
  opts.dates.day_first = false;
  const char kMin[] = "c OR{2{\\" "\0" "{:{c{N{;{5{2{A{c{A{2{,{2{O" "\x01" "from:j,c";
  expect_round_trip(std::string(kMin, sizeof kMin - 1));
}

TEST_F(Query, Bounds) {
  std::string long_text(s::kMaxQueryBytes + 100, 'a');
  auto r = parse(long_text);
  EXPECT_TRUE(has_hint(r, s::HintKind::kTruncated));
  EXPECT_LE(only(r).text.size(), s::kMaxQueryBytes);

  std::string many;
  for (int i = 0; i < 2000; ++i) many += "w" + std::to_string(i) + " ";
  many.resize(s::kMaxQueryBytes);
  r = parse(many);
  EXPECT_TRUE(has_hint(r, s::HintKind::kTruncated));
  size_t leaves = 0;
  for (const auto& n : r.query.nodes) leaves += (n.kind != s::NodeKind::kAnd && n.kind != s::NodeKind::kOr) ? 1 : 0;
  EXPECT_LE(leaves, s::kMaxNodes);
  expect_round_trip(many);

  r = parse(std::string(3000, '(') + "x" + std::string(3000, ')'));
  EXPECT_TRUE(has_hint(r, s::HintKind::kTruncated));
  EXPECT_FALSE(r.query.empty());
  r = parse(std::string(4000, '-') + "x");
  r = parse(std::string(4000, '"'));
  // Invalid UTF-8 and control characters are sanitized.
  std::string clean;
  r = s::parse_query(std::string("a\xff\xfe b\x01" "c"), opts, &clean);
  EXPECT_EQ(clean, "a\xEF\xBF\xBD\xEF\xBF\xBD b c");
}

// The parser never fails: random bytes and random soups of syntax pieces parse, stay within the
// bounds, and round-trip through canonical text.
TEST_F(Query, NeverFailsOnRandomInput) {
  std::mt19937 rng(20261114);  // NOLINT(cert-msc51-cpp): a fixed seed keeps failures reproducible
  const std::vector<std::string> pieces = {
      "a", "b", "OR", "AND", "AROUND", "3", "(", ")", "{", "}", "\"", "-", "+", "*", ",", ":", "\\", "?", " ", " ", " ",
      "from:", "to:", "subject:", "is:", "has:", "in:", "tag:", "date:", "before:", "after:", "within:", "notes:",
      "unread", "pdf", "trash", "anywhere", "me", "<x@y.z>", "last", "march", "may", "2024", "2024-07-05", "07/05/2024",
      "today", "since", "between", "and", "q3", "days", "ago", "2d", "1Week", "\xe4\xbc\x9a", "\xc3\xa9", "\xff", "\t",
  };
  for (int i = 0; i < 20000; ++i) {
    std::string text;
    if (i % 4 == 0) {
      const size_t n = rng() % 64;
      for (size_t k = 0; k < n; ++k) text.push_back(static_cast<char>(rng() % 256));
    } else {
      const size_t n = rng() % 24;
      for (size_t k = 0; k < n; ++k) text += pieces[rng() % pieces.size()];
    }
    const s::ParseResult a = parse(text);
    size_t leaves = 0;
    for (const auto& n : a.query.nodes) leaves += (n.kind != s::NodeKind::kAnd && n.kind != s::NodeKind::kOr) ? 1 : 0;
    ASSERT_LE(leaves, s::kMaxNodes);
    const std::string c = s::canonical_text(a.query, opts);
    const s::ParseResult b = parse(c);
    ASSERT_TRUE(s::same_query(a.query, b.query)) << "input: " << ::testing::PrintToString(text) << "\ncanonical: " << c
                                                 << "\nagain: " << s::canonical_text(b.query, opts);
  }
}

}  // namespace
