/* nomake test runner: collects the tests that tests.h registered, runs them and reports. */
#include "tests.h"

#include <setjmp.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#endif

#if defined(_WIN32) || defined(__EMSCRIPTEN__)
#define NM_JMP_BUF jmp_buf
#define NM_SETJMP(env) setjmp(env)
#define NM_LONGJMP(env, value) longjmp(env, value)
#else
#define NM_JMP_BUF sigjmp_buf
#define NM_SETJMP(env) sigsetjmp(env, 1)
#define NM_LONGJMP(env, value) siglongjmp(env, value)
#endif

typedef struct {
    const char *name;
    nm_test_fn fn;
    const char *file;
    int line;
} Test;

enum { RAN = 0, ABORTED = 1, SKIPPED = 2, CRASHED = 3 };

static Test *tests;
static size_t test_count;
static size_t test_capacity;

static NM_JMP_BUF jump;
static volatile int in_test;
static const Test *current;
static int failures;
static const char *skip_reason;
static char crash_text[64];

void nm_register(const char *name, nm_test_fn fn, const char *file, int line) {
    if (test_count == test_capacity) {
        test_capacity = test_capacity ? test_capacity * 2 : 64;
        tests = (Test *)realloc(tests, test_capacity * sizeof(Test));
        if (!tests) {
            fputs("nomake test: out of memory\n", stderr);
            exit(2);
        }
    }
    tests[test_count].name = name;
    tests[test_count].fn = fn;
    tests[test_count].file = file;
    tests[test_count].line = line;
    test_count++;
}

static void report_failure(const char *file, int line, const char *expr, const char *message) {
    failures++;
    printf("%s:%d: FAIL %s: %s", file, line, current ? current->name : "?", expr);
    if (message) printf(" (%s)", message);
    putchar('\n');
    fflush(stdout);
}

void nm_fail(const char *file, int line, const char *expr) {
    report_failure(file, line, expr, NULL);
}

void nm_fail_msg(const char *file, int line, const char *expr, const char *fmt, ...) {
    char message[512];
    va_list args;
    va_start(args, fmt);
    vsnprintf(message, sizeof message, fmt, args);
    va_end(args);
    report_failure(file, line, expr, message);
}

void nm_abort_test(void) {
    if (in_test) NM_LONGJMP(jump, ABORTED);
    exit(1);
}

void nm_skip(const char *reason) {
    skip_reason = reason;
    if (in_test) NM_LONGJMP(jump, SKIPPED);
}

#ifdef _WIN32
static LONG WINAPI on_exception(EXCEPTION_POINTERS *info) {
    DWORD code = info->ExceptionRecord->ExceptionCode;
    const char *name;
    switch (code) {
    case EXCEPTION_ACCESS_VIOLATION: name = "access violation"; break;
    case EXCEPTION_ILLEGAL_INSTRUCTION: name = "illegal instruction"; break;
    case EXCEPTION_INT_DIVIDE_BY_ZERO: name = "integer divide by zero"; break;
    case EXCEPTION_FLT_DIVIDE_BY_ZERO: name = "float divide by zero"; break;
    case EXCEPTION_ARRAY_BOUNDS_EXCEEDED: name = "array bounds exceeded"; break;
    case EXCEPTION_DATATYPE_MISALIGNMENT: name = "misaligned access"; break;
    case EXCEPTION_PRIV_INSTRUCTION: name = "privileged instruction"; break;
    default: return EXCEPTION_CONTINUE_SEARCH;
    }
    if (!in_test) return EXCEPTION_CONTINUE_SEARCH;
    snprintf(crash_text, sizeof crash_text, "%s (0x%08lx)", name, (unsigned long)code);
    NM_LONGJMP(jump, CRASHED);
    return EXCEPTION_CONTINUE_SEARCH;
}
#endif

#if !defined(__EMSCRIPTEN__)
static void on_signal(int sig) {
    if (!in_test) {
        signal(sig, SIG_DFL);
        raise(sig);
        return;
    }
    snprintf(crash_text, sizeof crash_text, "signal %d", sig);
    signal(sig, on_signal);
    NM_LONGJMP(jump, CRASHED);
}
#endif

static void install_crash_handlers(void) {
#ifdef _WIN32
    AddVectoredExceptionHandler(1, on_exception);
    _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
    _set_error_mode(_OUT_TO_STDERR);
#endif
#if !defined(__EMSCRIPTEN__)
    signal(SIGABRT, on_signal);
    signal(SIGFPE, on_signal);
    signal(SIGILL, on_signal);
    signal(SIGSEGV, on_signal);
#ifdef SIGBUS
    signal(SIGBUS, on_signal);
#endif
#endif
}

static int glob_match(const char *pattern, const char *text) {
    if (!*pattern) return !*text;
    if (*pattern == '*') {
        for (;; text++) {
            if (glob_match(pattern + 1, text)) return 1;
            if (!*text) return 0;
        }
    }
    if (!*text) return 0;
    if (*pattern != '?' && *pattern != *text) return 0;
    return glob_match(pattern + 1, text + 1);
}

/* A pattern with * or ? must match the whole name; a plain one matches anywhere in it. */
static int selected(const char *filter, const char *name) {
    if (!filter) return 1;
    if (strpbrk(filter, "*?")) return glob_match(filter, name);
    return strstr(name, filter) != NULL;
}

static int compare(const void *left, const void *right) {
    const Test *a = (const Test *)left;
    const Test *b = (const Test *)right;
    int by_file = strcmp(a->file, b->file);
    if (by_file) return by_file;
    return (a->line > b->line) - (a->line < b->line);
}

static void usage(void) {
    puts("usage: <tests> [--list] [--filter <name|glob>] [--verbose]");
}

int main(int argc, char **argv) {
    const char *filter = NULL;
    int list = 0;
    int verbose = 0;
    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];
        if (!strcmp(arg, "--list")) {
            list = 1;
        } else if (!strcmp(arg, "-v") || !strcmp(arg, "--verbose")) {
            verbose = 1;
        } else if (!strcmp(arg, "--filter") && i + 1 < argc) {
            filter = argv[++i];
        } else if (!strncmp(arg, "--filter=", 9)) {
            filter = arg + 9;
        } else if (!strcmp(arg, "-h") || !strcmp(arg, "--help")) {
            usage();
            return 0;
        } else {
            fprintf(stderr, "unknown argument '%s'\n", arg);
            usage();
            return 2;
        }
    }

    qsort(tests, test_count, sizeof(Test), compare);
    if (list) {
        for (size_t i = 0; i < test_count; i++) {
            if (selected(filter, tests[i].name)) puts(tests[i].name);
        }
        return 0;
    }
    install_crash_handlers();

    int passed = 0, failed = 0, skipped = 0, ran = 0;
    for (size_t i = 0; i < test_count; i++) {
        const Test *test = &tests[i];
        if (!selected(filter, test->name)) continue;
        ran++;
        current = test;
        failures = 0;
        skip_reason = NULL;
        if (verbose) printf("RUN  %s\n", test->name);
        fflush(stdout);

        in_test = 1;
        int outcome = NM_SETJMP(jump);
        if (outcome == RAN) test->fn();
        in_test = 0;

        if (outcome == SKIPPED) {
            skipped++;
            if (verbose) printf("SKIP %s: %s\n", test->name, skip_reason ? skip_reason : "");
        } else if (outcome == CRASHED) {
            failed++;
            printf("%s:%d: CRASH %s: %s\n", test->file, test->line, test->name, crash_text);
        } else if (failures > 0) {
            failed++;
        } else {
            passed++;
            if (verbose) printf("ok   %s\n", test->name);
        }
        fflush(stdout);
    }
    current = NULL;

    if (ran == 0) {
        puts(test_count ? "tests: no test matches the filter" : "tests: no tests registered");
        return 1;
    }
    printf("tests: %d passed, %d failed, %d skipped\n", passed, failed, skipped);
    return failed ? 1 : 0;
}
