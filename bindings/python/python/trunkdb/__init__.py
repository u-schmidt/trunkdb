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

from ._live import (
    Change,
    ChangeKind,
    Changed,
    Commit,
    Entered,
    Left,
    LiveEvent,
    RowChanged,
    RowEntered,
    RowEvent,
    RowLeft,
)
from ._trunkdb import (
    Changes,
    Collection,
    Cursor,
    Database,
    DocId,
    DuplicateIdError,
    DuplicateValueError,
    Error,
    FormatError,
    LiveAggregate,
    LiveQuery,
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
    "Change",
    "ChangeKind",
    "Changed",
    "Changes",
    "Collection",
    "Commit",
    "Cursor",
    "Database",
    "DocId",
    "DuplicateIdError",
    "DuplicateValueError",
    "Entered",
    "Error",
    "FormatError",
    "Left",
    "LiveAggregate",
    "LiveEvent",
    "LiveQuery",
    "MultipleMatchesError",
    "NotFoundError",
    "NotLiveError",
    "ParseError",
    "PreconditionFailedError",
    "ReadOnlyError",
    "RowChanged",
    "RowEntered",
    "RowEvent",
    "RowLeft",
    "Snapshot",
    "SnapshotTooOldError",
    "UpdateError",
    "Upserted",
    "View",
    "open",
    "version",
]
