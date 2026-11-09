/*
 * Every function of trunkdb.h called from C (SPEC §179): built against
 * the header, linked to the library, run under AddressSanitizer by
 * run.sh, so a leak or a read after free fails it.
 *
 *   every_function <a directory to write in>
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "trunkdb.h"

static int called = 0;

/* A call that must succeed. */
#define OK(call)                                                          \
    do {                                                                  \
        called++;                                                         \
        int32_t code_ = (call);                                           \
        if (code_ != TRUNKDB_OK) {                                        \
            fprintf(stderr, "%s:%d: %s: code %d: %s\n", __FILE__,         \
                    __LINE__, #call, code_, trunkdb_error_message());     \
            exit(1);                                                      \
        }                                                                 \
    } while (0)

/* Something that must hold. */
#define EXPECT(what)                                                      \
    do {                                                                  \
        if (!(what)) {                                                    \
            fprintf(stderr, "%s:%d: expected %s\n", __FILE__, __LINE__,   \
                    #what);                                               \
            exit(1);                                                      \
        }                                                                 \
    } while (0)

/* `text` contains `part`, then given back. */
static void has(char *text, const char *part) {
    if (text == NULL || strstr(text, part) == NULL) {
        fprintf(stderr, "expected \"%s\" in: %s\n", part, text ? text : "NULL");
        exit(1);
    }
    trunkdb_string_free(text);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: every_function <directory>\n");
        return 2;
    }
    char path[4096], backup[4096];
    snprintf(path, sizeof path, "%s/app.trunkdb", argv[1]);
    snprintf(backup, sizeof backup, "%s/backup.jsonl", argv[1]);

    EXPECT(trunkdb_abi_version() == TRUNKDB_ABI_VERSION);
    EXPECT(strlen(trunkdb_version()) > 0);

    trunkdb_db *db;
    OK(trunkdb_open(path, "{\"cache_size\": 8388608}", &db));

    char *id, *text;
    bool flag;
    uint64_t n;

    OK(trunkdb_index_ensure(db, "tasks", "[\"status\", \"due\"]", NULL, &flag));
    EXPECT(flag);
    OK(trunkdb_insert(db, "tasks", "{\"title\": \"write it\", \"status\": \"open\", \"due\": {\"$date\": \"2026-11-09\"}}", &id));
    OK(trunkdb_get(db, NULL, "tasks", id, &text));
    has(text, "\"$date\":\"2026-11-09\"");
    OK(trunkdb_replace(db, "tasks", id, "{\"title\": \"write it\", \"status\": \"open\", \"hours\": 3}", &flag));
    EXPECT(flag);
    OK(trunkdb_find(db, NULL, "tasks", "status = :s", "{\"s\": \"open\"}", &text));
    has(text, "\"hours\":3");
    OK(trunkdb_find_one(db, NULL, "tasks", "hours > 10", NULL, &text));
    EXPECT(text == NULL);
    OK(trunkdb_count(db, NULL, "tasks", NULL, NULL, &n));
    EXPECT(n == 1);
    OK(trunkdb_update(db, "tasks", "status = \"open\"", NULL, "{\"$inc\": {\"hours\": 2}}", &n));
    EXPECT(n == 1);
    OK(trunkdb_upsert(db, "visits", "page = \"/\"", NULL, "{\"$inc\": {\"hits\": 1}}", &text, &flag));
    EXPECT(flag);
    trunkdb_string_free(text);
    OK(trunkdb_aggregate(db, NULL, "tasks", NULL, "sum(hours) as total", NULL, &text));
    has(text, "\"total\":5");
    OK(trunkdb_explain(db, NULL, "tasks", "status = \"open\"", NULL, &text));
    has(text, "plan: index on");
    OK(trunkdb_indexes(db, "tasks", &text));
    has(text, "\"fields\":[\"status\",\"due\"]");

    OK(trunkdb_batch(db,
                     "{\"op\": \"insert\", \"collection\": \"log\", \"document\": {\"n\": 1}}\n"
                     "{\"op\": \"require\", \"collection\": \"tasks\", \"filter\": \"hours = 5\"}",
                     NULL, false, &text));
    has(text, "\"total\":1");

    trunkdb_snapshot *snapshot;
    OK(trunkdb_snapshot_open(db, &snapshot));
    OK(trunkdb_delete(db, "tasks", id, &flag));
    EXPECT(flag);
    trunkdb_cursor *cursor;
    OK(trunkdb_cursor_open(db, snapshot, "tasks", NULL, NULL, &cursor));
    int seen = 0;
    for (;;) {
        OK(trunkdb_cursor_next(cursor, &text));
        if (text == NULL) break;
        seen++;
        trunkdb_string_free(text);
    }
    EXPECT(seen == 1);
    OK(trunkdb_cursor_close(cursor));
    OK(trunkdb_snapshot_close(snapshot));

    OK(trunkdb_delete_many(db, "log", NULL, NULL, &n));
    EXPECT(n == 1);
    OK(trunkdb_collections(db, &text));
    has(text, "\"visits\"");
    OK(trunkdb_check(db, &text));
    has(text, "\"ok\":true");
    OK(trunkdb_stats(db, &text));
    has(text, "\"wal_bytes\"");
    OK(trunkdb_checkpoint(db));
    OK(trunkdb_compact(db, &text));
    has(text, "\"pages_after\"");
    OK(trunkdb_export(db, backup, &text));
    has(text, "\"ended\":true");
    OK(trunkdb_drop_collection(db, "visits", &flag));
    EXPECT(flag);
    OK(trunkdb_import(db, backup, NULL, &text));
    has(text, "\"documents\"");
    OK(trunkdb_index_drop(db, "tasks", "[\"status\", \"due\"]", &flag));
    EXPECT(flag);

    /* An error: its code, its message, and its details. */
    int32_t code = trunkdb_count(db, NULL, "tasks", "hours = = 1", NULL, &n);
    called++;
    EXPECT(code == TRUNKDB_PARSE);
    EXPECT(strstr(trunkdb_error_message(), "filter:") != NULL);
    EXPECT(strstr(trunkdb_error_detail(), "\"kind\":\"parse\"") != NULL);
    OK(trunkdb_close(NULL));

    trunkdb_string_free(id);
    trunkdb_string_free(NULL);
    OK(trunkdb_close(db));

    /* Every function, each at least once. */
    printf("ok: %d calls\n", called);
    return 0;
}
