/* nomake test support. Tests are registered automatically; the runner supplies main(). */
#ifndef NOMAKE_TESTS_H
#define NOMAKE_TESTS_H

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*nm_test_fn)(void);

void nm_register(const char *name, nm_test_fn fn, const char *file, int line);
void nm_fail(const char *file, int line, const char *expr);
void nm_fail_msg(const char *file, int line, const char *expr, const char *fmt, ...);
void nm_abort_test(void);
void nm_skip(const char *reason);

#ifdef __cplusplus
}
#endif

#define NM_CAT_(a, b) a##b
#define NM_CAT(a, b) NM_CAT_(a, b)

/* Runs a function before main: a static object in C++, a constructor on gcc/clang/clang-cl, the CRT init table on cl.exe. */
#if defined(__cplusplus)
#define NM_AUTOREGISTER(name)                                                         \
    namespace {                                                                       \
    struct NM_CAT(nm_reg_, name) {                                                    \
        NM_CAT(nm_reg_, name)() { nm_register(#name, NM_CAT(nm_test_, name), __FILE__, __LINE__); } \
    } NM_CAT(nm_reg_object_, name);                                                   \
    }
#elif defined(__GNUC__) || defined(__clang__)
#define NM_AUTOREGISTER(name)                                                         \
    __attribute__((constructor)) static void NM_CAT(nm_reg_, name)(void) {            \
        nm_register(#name, NM_CAT(nm_test_, name), __FILE__, __LINE__);               \
    }
#elif defined(_MSC_VER)
#pragma section(".CRT$XCU", read)
#ifdef _M_IX86
#define NM_SYMBOL_PREFIX "_"
#else
#define NM_SYMBOL_PREFIX ""
#endif
#define NM_AUTOREGISTER(name)                                                         \
    static void NM_CAT(nm_reg_, name)(void) {                                         \
        nm_register(#name, NM_CAT(nm_test_, name), __FILE__, __LINE__);               \
    }                                                                                 \
    __declspec(allocate(".CRT$XCU")) void (*NM_CAT(nm_reg_pointer_, name))(void) = NM_CAT(nm_reg_, name); \
    __pragma(comment(linker, "/include:" NM_SYMBOL_PREFIX "nm_reg_pointer_" #name))
#else
#error "nomake/tests.h does not know how to register tests with this compiler"
#endif

#define TEST(name)                          \
    static void NM_CAT(nm_test_, name)(void); \
    NM_AUTOREGISTER(name)                   \
    static void NM_CAT(nm_test_, name)(void)

/* TEST_CHECK records a failure and carries on, TEST_REQUIRE ends the test. */
#define TEST_CHECK(cond)                                       \
    do {                                                       \
        if (!(cond)) nm_fail(__FILE__, __LINE__, #cond);       \
    } while (0)

#define TEST_CHECK_MSG(cond, ...)                                          \
    do {                                                                   \
        if (!(cond)) nm_fail_msg(__FILE__, __LINE__, #cond, __VA_ARGS__);  \
    } while (0)

#define TEST_REQUIRE(cond)                                     \
    do {                                                       \
        if (!(cond)) {                                         \
            nm_fail(__FILE__, __LINE__, #cond);                \
            nm_abort_test();                                   \
        }                                                      \
    } while (0)

#define TEST_REQUIRE_MSG(cond, ...)                                        \
    do {                                                                   \
        if (!(cond)) {                                                     \
            nm_fail_msg(__FILE__, __LINE__, #cond, __VA_ARGS__);           \
            nm_abort_test();                                               \
        }                                                                  \
    } while (0)

#define TEST_SKIP(reason) nm_skip(reason)

#endif
