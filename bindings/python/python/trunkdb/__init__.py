"""trunkdb: an embedded, single-file document database (SPEC §197).

    import trunkdb

    with trunkdb.open("app.trunkdb") as db:
        tasks = db.collection("tasks")
        task_id = tasks.insert({"title": "Write it", "status": "open"})
        for task in tasks.find("status = :s", {"s": "open"}):
            print(task["_id"], task["title"])

Documents are dicts of Python's own types; ids are `DocId`; the texts
(filters, aggregates) are trunkdb's own, as the command takes them.
"""

from ._trunkdb import (
    Collection,
    Cursor,
    Database,
    DocId,
    DuplicateIdError,
    DuplicateValueError,
    Error,
    FormatError,
    MultipleMatchesError,
    NotFoundError,
    NotLiveError,
    ParseError,
    PreconditionFailedError,
    ReadOnlyError,
    Snapshot,
    SnapshotTooOldError,
    UpdateError,
    Upserted,
    View,
    open,
    version,
)

__version__ = version()

__all__ = [
    "Collection",
    "Cursor",
    "Database",
    "DocId",
    "DuplicateIdError",
    "DuplicateValueError",
    "Error",
    "FormatError",
    "MultipleMatchesError",
    "NotFoundError",
    "NotLiveError",
    "ParseError",
    "PreconditionFailedError",
    "ReadOnlyError",
    "Snapshot",
    "SnapshotTooOldError",
    "UpdateError",
    "Upserted",
    "View",
    "open",
    "version",
]
