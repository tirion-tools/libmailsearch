# The mail search query language (C++20, no I/O): the AST (query_ast.h), the parser and canonical
# text (query_parser.h) and dates (query_dates.h). Consumers add_subdirectory() this directory and
# link mailsearch_query; built on its own it also builds the tests (GoogleTest) with strict warnings
# as errors, and the libFuzzer target with -DMAILSEARCH_FUZZ=ON (clang).
cmake_minimum_required(VERSION 3.25)
project(mailsearch_query LANGUAGES CXX)

add_library(mailsearch_query STATIC src/query_dates.cc src/query_parser.cc)
target_include_directories(mailsearch_query PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_compile_features(mailsearch_query PUBLIC cxx_std_20)

if(PROJECT_IS_TOP_LEVEL)
  option(MAILSEARCH_FUZZ "Build the libFuzzer target (clang)" OFF)
  set(_strict -Wall -Wextra -Wpedantic -Wshadow -Wconversion -Wno-sign-conversion -Werror)
  target_compile_options(mailsearch_query PRIVATE ${_strict})
  if(MAILSEARCH_FUZZ)
    target_compile_options(mailsearch_query PRIVATE -fsanitize=fuzzer-no-link,address,undefined)
    add_executable(fuzz_query_parser fuzz/fuzz_query_parser.cc)
    target_link_libraries(fuzz_query_parser PRIVATE mailsearch_query)
    target_compile_options(fuzz_query_parser PRIVATE ${_strict} -fsanitize=fuzzer,address,undefined)
    target_link_options(fuzz_query_parser PRIVATE -fsanitize=fuzzer,address,undefined)
  else()
    find_package(GTest REQUIRED)
    include(GoogleTest)
    enable_testing()
    add_executable(query_parser_test tests/query_parser_test.cc)
    target_link_libraries(query_parser_test PRIVATE mailsearch_query GTest::gtest_main)
    target_compile_options(query_parser_test PRIVATE ${_strict})
    gtest_discover_tests(query_parser_test TEST_PREFIX "query.")
  endif()
endif()
