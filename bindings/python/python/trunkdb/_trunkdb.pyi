"""Type stubs of trunkdb's compiled module (SPEC §197.2)."""

import datetime
import os
from collections.abc import AsyncIterator, Iterable, Iterator, Mapping, Sequence
from types import TracebackType
from typing import Any, Literal, Self, TypeAlias, final

from ._live import Commit, LiveEvent, RowEvent

__all__ = [
    "Changes",
    "Collection",
    "Cursor",
    "Database",
    "DocId",
    "DuplicateIdError",
    "DuplicateValueError",
    "Error",
    "FormatError",
    "LiveAggregate",
    "LiveQuery",
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

_Value: TypeAlias = (
    None
    | bool
    | int
    | float
    | str
    | bytes
    | DocId
    | datetime.datetime
    | datetime.date
    | datetime.time
    | list[_Value]
    | dict[str, _Value]
)
"""A value as it comes out of trunkdb."""

_ValueIn: TypeAlias = (
    None
    | bool
    | int
    | float
    | str
    | bytes
    | bytearray
    | memoryview
    | DocId
    | datetime.datetime
    | datetime.date
    | datetime.time
    | Sequence[_ValueIn]
    | Mapping[str, _ValueIn]
)
"""A value trunkdb takes: a list or a tuple for an array."""

_Document: TypeAlias = dict[str, _Value]
_Params: TypeAlias = Mapping[str, _ValueIn] | None
_Path: TypeAlias = str | os.PathLike[str]

def open(
    path: _Path,
    *,
    read_only: bool = False,
    cache_size: int | None = None,
    snapshot_memory: int | None = None,
    checkpoint_pages: int | None = None,
    checkpoint_wal_bytes: int | None = None,
    watch_every: float | None = None,
) -> Database:
    """Opens the database at `path`, creating it if it isn't there."""

def version() -> str:
    """trunkdb's release, as "0.41.0"."""

@final
class DocId:
    """A document's id: equal, hashed and ordered as trunkdb orders ids."""

    def __new__(cls, text: str) -> Self: ...
    def __eq__(self, other: object, /) -> bool: ...
    def __lt__(self, other: DocId, /) -> bool: ...
    def __le__(self, other: DocId, /) -> bool: ...
    def __gt__(self, other: DocId, /) -> bool: ...
    def __ge__(self, other: DocId, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Upserted:
    """What an upsert wrote: the document's id, and whether it was inserted."""

    @property
    def id(self) -> DocId: ...
    @property
    def inserted(self) -> bool: ...

@final
class Database:
    """An open trunkdb database: from `trunkdb.open`."""

    @property
    def closed(self) -> bool: ...
    def collection(self, name: str) -> Collection: ...
    def collections(self) -> list[str]: ...
    def drop_collection(self, name: str) -> bool: ...
    def snapshot(self) -> Snapshot: ...
    def subscribe(self) -> Changes: ...
    def batch(
        self,
        ops: list[Mapping[str, _ValueIn]] | str,
        params: _Params = None,
        *,
        dry_run: bool = False,
    ) -> dict[str, Any]: ...
    def checkpoint(self) -> None: ...
    def compact(self) -> dict[str, Any]: ...
    def check(self) -> dict[str, Any]: ...
    def stats(self) -> dict[str, Any]: ...
    def export(self, path: _Path) -> dict[str, Any]: ...
    def import_file(self, path: _Path, *, max_line_bytes: int | None = None) -> dict[str, Any]: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

@final
class Collection:
    """A collection of a database, to read and write."""

    @property
    def name(self) -> str: ...
    def get(self, id: DocId | str) -> _Document | None: ...
    def find(self, filter: str = "", params: _Params = None) -> list[_Document]: ...
    def find_one(self, filter: str = "", params: _Params = None) -> _Document | None: ...
    def count(self, filter: str = "", params: _Params = None) -> int: ...
    def cursor(self, filter: str = "", params: _Params = None) -> Cursor: ...
    def aggregate(self, filter: str, aggregate: str, params: _Params = None) -> list[_Document]: ...
    def explain(self, filter: str = "", params: _Params = None) -> str: ...
    def insert(self, doc: Mapping[str, _ValueIn]) -> DocId: ...
    def insert_many(self, docs: Iterable[Mapping[str, _ValueIn]]) -> list[DocId]: ...
    def update(self, id: DocId | str, doc: Mapping[str, _ValueIn]) -> bool: ...
    def update_many(
        self, filter: str, update: Mapping[str, _ValueIn], params: _Params = None
    ) -> int: ...
    def delete(self, id: DocId | str) -> bool: ...
    def delete_many(self, filter: str, params: _Params = None) -> int: ...
    def upsert(self, filter: str, update: Mapping[str, _ValueIn], params: _Params = None) -> Upserted: ...
    def watch(self, filter: str = "", params: _Params = None) -> LiveQuery: ...
    def watch_aggregate(
        self, filter: str, aggregate: str, params: _Params = None
    ) -> LiveAggregate: ...
    def ensure_index(
        self, fields: str | Sequence[str], *, unique: bool = False, sparse: bool = False
    ) -> bool: ...
    def drop_index(self, fields: str | Sequence[str]) -> bool: ...
    def indexes(self) -> list[dict[str, Any]]: ...
    def drop(self) -> bool: ...

@final
class Snapshot:
    """The database as it was at one commit: from `Database.snapshot()`."""

    @property
    def closed(self) -> bool: ...
    def collection(self, name: str) -> View: ...
    def collections(self) -> list[str]: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

@final
class View:
    """A collection as a snapshot holds it, to read."""

    @property
    def name(self) -> str: ...
    def get(self, id: DocId | str) -> _Document | None: ...
    def find(self, filter: str = "", params: _Params = None) -> list[_Document]: ...
    def find_one(self, filter: str = "", params: _Params = None) -> _Document | None: ...
    def count(self, filter: str = "", params: _Params = None) -> int: ...
    def cursor(self, filter: str = "", params: _Params = None) -> Cursor: ...
    def aggregate(self, filter: str, aggregate: str, params: _Params = None) -> list[_Document]: ...
    def explain(self, filter: str = "", params: _Params = None) -> str: ...
    def indexes(self) -> list[dict[str, Any]]: ...

@final
class Cursor:
    """A read handed out a document at a time; closes itself at its end."""

    @property
    def closed(self) -> bool: ...
    def __iter__(self) -> Iterator[_Document]: ...
    def __next__(self) -> _Document: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

@final
class LiveQuery:
    """A live query, from `Collection.watch`."""

    @property
    def ended(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    def next(self, timeout: float | None = None) -> LiveEvent | None: ...
    def close(self) -> None: ...
    def __iter__(self) -> Iterator[LiveEvent]: ...
    def __next__(self) -> LiveEvent: ...
    def __aiter__(self) -> AsyncIterator[LiveEvent]: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

@final
class LiveAggregate:
    """A live aggregate, from `Collection.watch_aggregate`."""

    @property
    def ended(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    def next(self, timeout: float | None = None) -> RowEvent | None: ...
    def close(self) -> None: ...
    def __iter__(self) -> Iterator[RowEvent]: ...
    def __next__(self) -> RowEvent: ...
    def __aiter__(self) -> AsyncIterator[RowEvent]: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

@final
class Changes:
    """A subscription, from `Database.subscribe`."""

    @property
    def ended(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    def next(self, timeout: float | None = None) -> Commit | None: ...
    def close(self) -> None: ...
    def __iter__(self) -> Iterator[Commit]: ...
    def __next__(self) -> Commit: ...
    def __aiter__(self) -> AsyncIterator[Commit]: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> Literal[False]: ...

class Error(Exception):
    """Every error of trunkdb's own; `kind` names which."""

    kind: str

class ParseError(Error):
    """A text that doesn't read."""

    what: str
    text: str | None
    offset: int | None
    line: int | None
    parameter: str | None

class DuplicateIdError(Error):
    collection: str
    id: DocId

class DuplicateValueError(Error):
    collection: str
    field: str
    id: DocId
    existing: DocId

class NotFoundError(Error):
    collection: str
    id: DocId

class MultipleMatchesError(Error):
    collection: str
    count: int

class UpdateError(Error):
    collection: str
    id: DocId
    message: str

class PreconditionFailedError(Error):
    collection: str
    op: int
    matched: bool
    line: int | None

class NotLiveError(Error):
    reason: str

class SnapshotTooOldError(Error): ...
class ReadOnlyError(Error): ...
class FormatError(Error): ...
