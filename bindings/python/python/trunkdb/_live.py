"""Changes as they happen (SPEC §197.8): the events the feeds hand out,
and `async for` over a feed.

The events are frozen dataclasses, matched as Rust's `match` matches:

    for event in tasks.watch('status = "open"'):
        match event:
            case trunkdb.Entered(id=id, doc=doc):
                ...
            case trunkdb.Left(id=id):
                ...
"""

from __future__ import annotations

import asyncio
import enum
import queue
import threading
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Generic, TypeVar, Union

if TYPE_CHECKING:
    from ._trunkdb import DocId

__all__ = [
    "Change",
    "ChangeKind",
    "Changed",
    "Commit",
    "Entered",
    "Left",
    "LiveEvent",
    "RowChanged",
    "RowEntered",
    "RowEvent",
    "RowLeft",
]


@dataclass(frozen=True, slots=True)
class Entered:
    """A document that came to match the query, or matched when it was
    made. With an `order by`, `at` is its place in the list: put in
    there."""

    id: DocId
    doc: dict[str, Any]
    at: int | None = None


@dataclass(frozen=True, slots=True)
class Left:
    """A document that matched and no longer does; or, with a `limit`,
    still does, and others came before it. `doc` is `None` if it was
    deleted. With an `order by`, `from_` is the place it had: taken out
    there."""

    id: DocId
    doc: dict[str, Any] | None
    from_: int | None = None


@dataclass(frozen=True, slots=True)
class Changed:
    """A document that matched, still does, and was written. With an
    `order by`, taken out at `from_` and put in at `at`."""

    id: DocId
    doc: dict[str, Any]
    from_: int | None = None
    at: int | None = None


@dataclass(frozen=True, slots=True)
class RowEntered:
    """A row that came into the aggregate's result: `group` its group
    fields' values, `row` the whole row."""

    group: dict[str, Any]
    row: dict[str, Any]


@dataclass(frozen=True, slots=True)
class RowLeft:
    """A row that left the result; `row` as it is now, `None` if its group
    has no documents left."""

    group: dict[str, Any]
    row: dict[str, Any] | None


@dataclass(frozen=True, slots=True)
class RowChanged:
    """A row still in the result whose values changed."""

    group: dict[str, Any]
    row: dict[str, Any]


class ChangeKind(enum.StrEnum):
    """What a commit did to a document, or to a collection."""

    INSERTED = "inserted"
    UPDATED = "updated"
    DELETED = "deleted"
    DROPPED = "dropped"


@dataclass(frozen=True, slots=True)
class Change:
    """One document a commit inserted, updated or deleted, or a collection
    it dropped (`id` then `None`)."""

    kind: ChangeKind
    collection: str
    id: DocId | None


@dataclass(frozen=True, slots=True)
class Commit:
    """What one committed batch changed, each document once, in the order
    the batch first touched it."""

    changes: tuple[Change, ...]


LiveEvent = Union[Entered, Left, Changed]
"""What a `LiveQuery` hands out."""

RowEvent = Union[RowEntered, RowLeft, RowChanged]
"""What a `LiveAggregate` hands out."""

_E = TypeVar("_E")


class _AsyncReader(Generic[_E]):
    """`async for` over a feed: each `__anext__` hands one wait to a
    thread of the loop's own, which reads one event, never one ahead, so
    a loop left by `break` leaves nothing read behind. A wait cancelled
    once its event came puts the event back in front of the feed."""

    def __init__(self, feed: Any) -> None:
        self._feed = feed
        self._waits: queue.SimpleQueue[
            tuple[Any, asyncio.AbstractEventLoop, asyncio.Future[None]] | None
        ] = queue.SimpleQueue()
        self._thread: threading.Thread | None = None

    def __aiter__(self) -> _AsyncReader[_E]:
        return self

    async def __anext__(self) -> _E:
        loop = asyncio.get_running_loop()
        if self._thread is None:
            # The thread holds the queue, not the reader: once the loop
            # lets go of the reader, `__del__` ends the thread.
            self._thread = threading.Thread(
                target=_serve, args=(self._waits,), name="trunkdb-feed", daemon=True
            )
            self._thread.start()
        wait = self._feed._wait()
        came: asyncio.Future[None] = loop.create_future()
        self._waits.put((wait, loop, came))
        try:
            await came
        except BaseException:
            wait.abandon()
            raise
        event: _E = wait.take()
        return event

    def __del__(self) -> None:
        self._waits.put(None)


def _serve(
    waits: queue.SimpleQueue[
        tuple[Any, asyncio.AbstractEventLoop, asyncio.Future[None]] | None
    ],
) -> None:
    while (each := waits.get()) is not None:
        wait, loop, came = each
        try:
            wait.serve()
        finally:
            try:
                loop.call_soon_threadsafe(_resolve, came)
            except RuntimeError:
                # The loop closed: nobody waits for it.
                wait.abandon()


def _resolve(came: asyncio.Future[None]) -> None:
    if not came.done():
        came.set_result(None)
