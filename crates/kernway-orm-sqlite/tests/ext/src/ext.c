/* A test extension written the way SQLite extensions are: against
 * sqlite3ext.h, every SQLite call through the API table the loader passes. */
#include "sqlite3ext.h"
#include <string.h>
SQLITE_EXTENSION_INIT1

static void version_fn(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
    (void)argc; (void)argv;
    sqlite3_result_text(ctx, "kernway-test-ext 1", -1, SQLITE_STATIC);
}

/* Reverse a UTF-8 string by characters (not bytes). */
static void reverse_fn(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
    (void)argc;
    if (sqlite3_value_type(argv[0]) == SQLITE_NULL) {
        sqlite3_result_null(ctx);
        return;
    }
    const unsigned char *s = sqlite3_value_text(argv[0]);
    int n = sqlite3_value_bytes(argv[0]);
    char *out = sqlite3_malloc(n + 1);
    if (!out) {
        sqlite3_result_error_nomem(ctx);
        return;
    }
    int end = n, w = 0;
    while (end > 0) {
        int start = end - 1;
        while (start > 0 && (s[start] & 0xC0) == 0x80) start--;
        memcpy(out + w, s + start, (size_t)(end - start));
        w += end - start;
        end = start;
    }
    sqlite3_result_text(ctx, out, w, sqlite3_free);
}

static void alt_fn(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
    (void)argc; (void)argv;
    sqlite3_result_text(ctx, "alternate entry point", -1, SQLITE_STATIC);
}

int kw_c_init(sqlite3 *db, char **err, const sqlite3_api_routines *api) {
    (void)err;
    SQLITE_EXTENSION_INIT2(api);
    int flags = SQLITE_UTF8 | SQLITE_DETERMINISTIC;
    int rc = sqlite3_create_function_v2(db, "kw_ext_version", 0, flags, 0, version_fn, 0, 0, 0);
    if (rc == SQLITE_OK) rc = sqlite3_create_function_v2(db, "kw_reverse", 1, flags, 0, reverse_fn, 0, 0, 0);
    return rc;
}

int kw_c_alt_init(sqlite3 *db, char **err, const sqlite3_api_routines *api) {
    (void)err;
    SQLITE_EXTENSION_INIT2(api);
    return sqlite3_create_function_v2(db, "kw_ext_alt", 0, SQLITE_UTF8 | SQLITE_DETERMINISTIC, 0, alt_fn, 0, 0, 0);
}
