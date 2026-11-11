/*
 * trunkdb's C ABI (SPEC §177, §179 in the trunkdb repository).
 *
 * The rules every function keeps:
 *
 * - Text, in and out, is UTF-8 ending in a zero byte. Text passed in is
 *   only read, during the call. Documents are JSON as an export writes
 *   it: {"$id": ...}, {"$date": ...}, {"$time": ...}, {"$binary": ...}.
 *   Filters and aggregates are text, `status = :s order by due`, their
 *   parameters' values a JSON object, {"s": "open"}, or NULL.
 * - Every function but the frees returns TRUNKDB_OK or an error code.
 *   trunkdb_error_message() and trunkdb_error_detail() say more, for the
 *   last error on the calling thread.
 * - Results come back through the last parameters. Each is set to NULL,
 *   0 or false before anything else, and is written only on success.
 *   A `char *` handed out is the caller's: give it back with
 *   trunkdb_string_free(), never with free().
 * - Handles (trunkdb_db, trunkdb_snapshot, trunkdb_cursor) come from an
 *   _open function and go back to its _close. A handle of another kind,
 *   or a closed one, is refused where it can be told, never trusted:
 *   don't pass one. Closing NULL does nothing.
 * - A database handle, or a snapshot, may be used from any number of
 *   threads at once; a cursor from one at a time.
 * - No panic crosses into C: one is TRUNKDB_PANIC.
 */

#ifndef TRUNKDB_H
#define TRUNKDB_H

/* Made by cbindgen from trunkdb-ffi/src: change the Rust, not this file. */

#include <stdbool.h>
#include <stdint.h>

// The version of these functions: a library whose `trunkdb_abi_version`
// differs from the header's is not the one the program was built for.
#define TRUNKDB_ABI_VERSION 1

// The call did what it says.
#define TRUNKDB_OK 0

// A pointer or text that isn't what the call takes: null where a value
// is needed, text that isn't UTF-8, a handle of another kind or closed;
// or what the library refuses as asked, as an index on fields that have
// one of other options, or a batch's precondition with a limit.
#define TRUNKDB_INVALID_ARGUMENT 1

// A text that doesn't read: a filter, an aggregate, a document, an
// update, parameters, an id, options or a batch's line.
#define TRUNKDB_PARSE 2

// The file system's error, or a file that is damaged.
#define TRUNKDB_IO 3

// A file that isn't trunkdb's, or of a format this build doesn't read.
#define TRUNKDB_FORMAT 4

// A write to a database opened read-only.
#define TRUNKDB_READ_ONLY 5

// A failed write left the database unusable until it is opened again.
#define TRUNKDB_POISONED 6

// An insert of an id the collection has.
#define TRUNKDB_DUPLICATE_ID 7

// A unique index refused a value another document has.
#define TRUNKDB_DUPLICATE_VALUE 8

// A batch's op on an id the collection doesn't have.
#define TRUNKDB_NOT_FOUND 9

// An upsert whose filter matches more than one document.
#define TRUNKDB_MULTIPLE_MATCHES 10

// An update's operator that can't be made to a document.
#define TRUNKDB_UPDATE 11

// A batch's precondition failed.
#define TRUNKDB_PRECONDITION_FAILED 12

// A document that isn't an object.
#define TRUNKDB_NOT_AN_OBJECT 13

// An index hint that can't be followed.
#define TRUNKDB_HINT 14

// A line of an import that isn't what an export writes.
#define TRUNKDB_IMPORT 15

// A compaction while a snapshot is open.
#define TRUNKDB_SNAPSHOT_OPEN 16

// A snapshot kept past the memory snapshots may take.
#define TRUNKDB_SNAPSHOT_TOO_OLD 17

// A panic inside trunkdb, caught at the boundary.
#define TRUNKDB_PANIC 18

// An error of a kind added after this header.
#define TRUNKDB_OTHER 19

// A read handed out a document at a time: from `trunkdb_cursor_open`,
// closed by `trunkdb_cursor_close`.
typedef struct trunkdb_cursor trunkdb_cursor;

// An open database: from `trunkdb_open`, closed by `trunkdb_close`. Any
// number of threads may use one at once.
typedef struct trunkdb_db trunkdb_db;

// The database as it was at one commit: from `trunkdb_snapshot_open`,
// closed by `trunkdb_snapshot_close`. Any number of threads may read it
// at once.
typedef struct trunkdb_snapshot trunkdb_snapshot;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// The version of these functions the library was built with:
// `TRUNKDB_ABI_VERSION` of its header.
uint32_t trunkdb_abi_version(void);

// trunkdb's release, "0.39.0": owned by the library, never freed.
const char *trunkdb_version(void);

// The message of this thread's last error, as text: owned by the
// library, valid until the thread's next call that fails. Empty before
// any has.
const char *trunkdb_error_message(void);

// This thread's last error as JSON: `code`, `kind`, `message`, and what
// else the error knows, such as `collection`, `field`, `id`, `line` or
// `offset`. Owned by the library, valid as `trunkdb_error_message`.
const char *trunkdb_error_detail(void);

// Gives back a string a call handed out. Null does nothing. Never pass
// one to C's `free`, nor a string the library didn't hand out.
void trunkdb_string_free(char *text);

// Opens the database at `path`, creating it if it isn't there.
// `options`, JSON or null: `read_only` (true or false), `cache_size`,
// `snapshot_memory` (bytes), `checkpoint_pages`, `checkpoint_wal_bytes`.
int32_t trunkdb_open(const char *path,
                     const char *options,
                     trunkdb_db **db);

// Closes `db`. The file closes once its snapshots and cursors are closed
// too. Null does nothing.
int32_t trunkdb_close(trunkdb_db *db);

// Writes what the log holds into the file, and empties the log.
int32_t trunkdb_checkpoint(const trunkdb_db *db);

// Rebuilds the file without its free pages: `{"pages_before": …,
// "pages_after": …}`. Fails while a snapshot is open.
int32_t trunkdb_compact(const trunkdb_db *db,
                        char **report);

// Reads the whole file and checks it: `{"ok": …, "collections": …,
// "documents": …, "pages": …, "problems": […]}`. Problems found are
// not an error.
int32_t trunkdb_check(const trunkdb_db *db,
                      char **report);

// Where the file's space goes: the file, its pages by use, and each
// collection's documents, sizes and indexes, as JSON.
int32_t trunkdb_stats(const trunkdb_db *db,
                      char **report);

// The collections' names, a JSON array in name order.
int32_t trunkdb_collections(const trunkdb_db *db,
                            char **names);

// Drops a collection, its documents and indexes; `dropped` says whether
// it was there.
int32_t trunkdb_drop_collection(const trunkdb_db *db,
                                const char *collection,
                                bool *dropped);

// Exports every collection to the file at `path` as JSON lines, written
// beside it and renamed over it once complete: `{"collections": …,
// "documents": …, "ended": true}`.
int32_t trunkdb_export(const trunkdb_db *db,
                       const char *path,
                       char **summary);

// Imports an export from the file at `path`, as one batch. `options`,
// JSON or null: `max_line_bytes`.
int32_t trunkdb_import(const trunkdb_db *db,
                       const char *path,
                       const char *options,
                       char **summary);

// Makes an index on `fields` unless there is one; `created` says
// whether this call made it. `options`, JSON or null: `unique`,
// `sparse`.
int32_t trunkdb_index_ensure(const trunkdb_db *db,
                             const char *collection,
                             const char *fields,
                             const char *options,
                             bool *created);

// Drops the index on `fields`; `dropped` says whether there was one.
int32_t trunkdb_index_drop(const trunkdb_db *db,
                           const char *collection,
                           const char *fields,
                           bool *dropped);

// A collection's indexes: a JSON array of `{"fields": […], "unique": …,
// "sparse": …}`.
int32_t trunkdb_indexes(const trunkdb_db *db,
                        const char *collection,
                        char **indexes);

// The document with `id`, as JSON; null if there is none.
int32_t trunkdb_get(const trunkdb_db *db,
                    const trunkdb_snapshot *snapshot,
                    const char *collection,
                    const char *id,
                    char **document);

// What `filter` finds, a JSON array of documents. `filter` is a filter's
// text, null or empty for every document; `params` its parameters'
// values as a JSON object, or null.
int32_t trunkdb_find(const trunkdb_db *db,
                     const trunkdb_snapshot *snapshot,
                     const char *collection,
                     const char *filter,
                     const char *params,
                     char **documents);

// The first document `filter` finds, as JSON; null if none.
int32_t trunkdb_find_one(const trunkdb_db *db,
                         const trunkdb_snapshot *snapshot,
                         const char *collection,
                         const char *filter,
                         const char *params,
                         char **document);

// How many documents `filter` finds.
int32_t trunkdb_count(const trunkdb_db *db,
                      const trunkdb_snapshot *snapshot,
                      const char *collection,
                      const char *filter,
                      const char *params,
                      uint64_t *count);

// An aggregate's rows over what `filter` finds, a JSON array:
// `aggregate` as text, `count() as n group by team`. `params` holds
// the values of both texts' parameters, each taking its own.
int32_t trunkdb_aggregate(const trunkdb_db *db,
                          const trunkdb_snapshot *snapshot,
                          const char *collection,
                          const char *filter,
                          const char *aggregate,
                          const char *params,
                          char **rows);

// Runs `filter` and says how: its plan, what it read and how much, as
// the lines `trunkdb find --explain` prints.
int32_t trunkdb_explain(const trunkdb_db *db,
                        const trunkdb_snapshot *snapshot,
                        const char *collection,
                        const char *filter,
                        const char *params,
                        char **explained);

// Inserts a document, a JSON object; `id` is its id, the one its `_id`
// gives or a new one.
int32_t trunkdb_insert(const trunkdb_db *db,
                       const char *collection,
                       const char *document,
                       char **id);

// Replaces the document with `id` by `document`, whole; `found` says
// whether there was one. None is inserted.
int32_t trunkdb_replace(const trunkdb_db *db,
                        const char *collection,
                        const char *id,
                        const char *document,
                        bool *found);

// Deletes the document with `id`; `found` says whether there was one.
int32_t trunkdb_delete(const trunkdb_db *db,
                       const char *collection,
                       const char *id,
                       bool *found);

// Makes `update`, MongoDB's operators as JSON, `{"$set": {"done":
// true}}`, to what `filter` finds; `count` is how many it changed. Null
// or empty `filter` is every document.
int32_t trunkdb_update(const trunkdb_db *db,
                       const char *collection,
                       const char *filter,
                       const char *params,
                       const char *update,
                       uint64_t *count);

// Deletes what `filter` finds; `count` is how many. Null or empty
// `filter` is every document.
int32_t trunkdb_delete_many(const trunkdb_db *db,
                            const char *collection,
                            const char *filter,
                            const char *params,
                            uint64_t *count);

// Makes `update` to the one document `filter` finds, or inserts one
// made of the filter's `=`s and then `update`: `id` is the document's,
// `inserted` whether it was made. Several matches fail.
int32_t trunkdb_upsert(const trunkdb_db *db,
                       const char *collection,
                       const char *filter,
                       const char *params,
                       const char *update,
                       char **id,
                       bool *inserted);

// Runs a batch of ops, one JSON object a line, as `trunkdb batch` reads
// them, all committed or none; with `dry_run`, each run and rolled
// back. `params` holds the values for every line's filter. `written`
// is `{"ops": [{"line": 1, "op": "insert", "count": 1, "id": …}, …],
// "total": …, "dry_run": …}`: each op's line, what it wrote, an
// insert's or an upsert's id (not on a dry run) and an upsert's
// `inserted`. A failure's details name its `line`.
int32_t trunkdb_batch(const trunkdb_db *db,
                      const char *lines,
                      const char *params,
                      bool dry_run,
                      char **written);

// The database as it is now, kept for reads however it is written to
// after: pass it to a read instead of null. Close it soon; while open,
// it keeps what was written since in memory.
int32_t trunkdb_snapshot_open(const trunkdb_db *db,
                              trunkdb_snapshot **snapshot);

// Closes a snapshot. Null does nothing.
int32_t trunkdb_snapshot_close(trunkdb_snapshot *snapshot);

// What `filter` finds, handed out by `trunkdb_cursor_next` a document
// at a time, of the database as it was when opened.
int32_t trunkdb_cursor_open(const trunkdb_db *db,
                            const trunkdb_snapshot *snapshot,
                            const char *collection,
                            const char *filter,
                            const char *params,
                            trunkdb_cursor **cursor);

// The cursor's next document, as JSON; null at the end.
int32_t trunkdb_cursor_next(const trunkdb_cursor *cursor,
                            char **document);

// Closes a cursor. Null does nothing.
int32_t trunkdb_cursor_close(trunkdb_cursor *cursor);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* TRUNKDB_H */
