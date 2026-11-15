"""Changes as they happen (SPEC §197.8): live queries, live aggregates and
subscriptions, read by `next`, `for` and `async for`."""

import _thread
import asyncio
import dataclasses
import subprocess
import sys
import textwrap
import threading
import time

import pytest

import trunkdb
from trunkdb import (
    Change,
    ChangeKind,
    Changed,
    Commit,
    Entered,
    Left,
    RowChanged,
    RowEntered,
    RowLeft,
)


def later(seconds, call):
    """`call` on a thread of its own after `seconds`."""
    timer = threading.Timer(seconds, call)
    timer.start()
    return timer


def test_a_live_query_reports_what_comes_and_goes(tasks):
    ada = tasks.insert({"name": "ada", "status": "open"})
    with tasks.watch("status = :s", {"s": "open"}) as open_:
        assert open_.next(0) == Entered(ada, tasks.get(ada), None)
        assert open_.next(0) is None and not open_.ended

        bo = tasks.insert({"name": "bo", "status": "open"})
        tasks.update_many('name = "ada"', {"$set": {"seen": True}})
        tasks.update_many('name = "bo"', {"$set": {"status": "done"}})
        tasks.delete(ada)
        tasks.insert({"name": "cy", "status": "done"})

        seen = []
        while (event := open_.next(0)) is not None:
            match event:
                case Entered(id=id, doc=doc):
                    seen.append(("entered", doc["name"]))
                case Changed(id=id, doc=doc, from_=None, at=None):
                    seen.append(("changed", doc["seen"]))
                case Left(id=id, doc=None):
                    seen.append(("deleted", id))
                case Left(id=id, doc=doc):
                    seen.append(("left", doc["status"]))
        assert seen == [
            ("entered", "bo"),
            ("changed", True),
            ("left", "done"),
            ("deleted", ada),
        ]
    assert open_.closed and open_.ended
    assert tasks.get(bo)["status"] == "done"


def test_a_sorted_live_query_says_where_in_its_list(tasks):
    for n in (5, 1, 3):
        tasks.insert({"n": n})
    with tasks.watch("n > 0 order by n limit 2") as first_two:
        assert [(e.doc["n"], e.at) for e in (first_two.next(0), first_two.next(0))] == [
            (1, 0),
            (3, 1),
        ]
        tasks.insert({"n": 2})
        left, entered = first_two.next(1), first_two.next(1)
        assert (type(left), left.doc["n"], left.from_) == (Left, 3, 1)
        assert (type(entered), entered.doc["n"], entered.at) == (Entered, 2, 1)
        tasks.update_many("n = 1", {"$set": {"n": 2.5}})
        moved = first_two.next(1)
        assert (type(moved), moved.doc["n"], moved.from_, moved.at) == (Changed, 2.5, 0, 1)


def test_a_filter_that_cant_be_live_is_refused(tasks):
    with pytest.raises(trunkdb.NotLiveError) as caught:
        tasks.watch("n > 0 limit 1")
    assert caught.value.reason == "a limit without a sort"
    with pytest.raises(trunkdb.ParseError) as caught:
        tasks.watch("n = :n", {"n": 1, "x": 2})
    assert caught.value.parameter == "x"


def test_a_live_aggregate_reports_its_rows(tasks):
    tasks.insert({"team": "a", "hours": 2})
    with tasks.watch_aggregate(
        "", "sum(hours) as total group by team having total > :over", {"over": 1}
    ) as busy:
        assert busy.next(0) == RowEntered({"team": "a"}, {"team": "a", "total": 2})
        tasks.insert({"team": "a", "hours": 3})
        assert busy.next(1) == RowChanged({"team": "a"}, {"team": "a", "total": 5})
        tasks.insert({"team": "b", "hours": 1})
        assert busy.next(0.05) is None
        tasks.delete_many('team = "a"')
        assert busy.next(1) == RowLeft({"team": "a"}, None)


def test_a_subscription_reports_each_commit(db, tasks):
    with db.subscribe() as changes:
        ada = tasks.insert({"name": "ada"})
        tasks.update(ada, {"name": "ada", "n": 1})
        db.batch(
            [
                {"op": "delete", "collection": "tasks", "filter": 'name = "ada"'},
                {"op": "insert", "collection": "other", "document": {}},
            ]
        )
        db.drop_collection("other")
        commits = [changes.next(1) for _ in range(4)]
        assert changes.next(0) is None
    assert commits[0] == Commit((Change(ChangeKind.INSERTED, "tasks", ada),))
    assert commits[1].changes[0].kind == "updated"
    kinds = [(c.kind, c.collection) for c in commits[2].changes]
    assert kinds == [("deleted", "tasks"), ("inserted", "other")]
    assert commits[3] == Commit((Change(ChangeKind.DROPPED, "other", None),))


def test_events_are_frozen(tasks):
    tasks.insert({"n": 1})
    with tasks.watch() as live:
        event = live.next(0)
    with pytest.raises(dataclasses.FrozenInstanceError):
        event.doc = {}
    assert repr(event).startswith("Entered(id=DocId(")


def test_next_waits_as_long_as_its_timeout(tasks):
    with tasks.watch() as live:
        start = time.monotonic()
        assert live.next(0.3) is None
        assert 0.25 < time.monotonic() - start < 3
        with pytest.raises(ValueError, match="timeout"):
            live.next(-1)
        with pytest.raises(ValueError, match="timeout"):
            live.next(float("nan"))


def test_a_wait_lets_other_threads_write(tasks):
    # The writer can only commit while the wait has let the GIL go.
    with tasks.watch() as live:
        later(0.1, lambda: tasks.insert({"n": 1}))
        event = live.next()
    assert event.doc["n"] == 1


def test_ctrl_c_ends_a_wait(tasks):
    with tasks.watch() as live:
        timer = later(0.2, _thread.interrupt_main)
        start = time.monotonic()
        with pytest.raises(KeyboardInterrupt):
            live.next()
        timer.join()
    assert time.monotonic() - start < 5


def test_for_reads_until_the_feed_is_closed_on_another_thread(tasks):
    live = tasks.watch()
    tasks.insert({"n": 1})
    tasks.insert({"n": 2})
    later(0.3, live.close)
    assert [e.doc["n"] for e in live] == [1, 2]
    assert live.closed and live.ended
    assert live.next() is None
    assert repr(live) == "<trunkdb.LiveQuery closed 'tasks'>"


def test_a_closed_database_ends_its_feeds_once_theyre_read(tmp_path):
    db = trunkdb.open(tmp_path / "e.trunkdb")
    tasks = db.collection("tasks")
    live = tasks.watch()
    rows = tasks.watch_aggregate("", "count() as n")
    changes = db.subscribe()
    tasks.insert({"n": 1})
    db.close()
    assert live.next(1).doc["n"] == 1
    assert not live.ended
    assert live.next() is None and live.ended
    assert [type(e) for e in rows] == [RowEntered, RowChanged]
    assert [len(c.changes) for c in changes] == [1]
    assert changes.ended and not changes.closed
    assert repr(changes) == "<trunkdb.Changes>"


def test_a_feed_left_open_doesnt_warn(tasks):
    # It holds no file: freed, it ends, as a dropped Rust feed does.
    live = tasks.watch()
    del live
    for _ in tasks.watch_aggregate("", "count() as n"):
        break


def test_async_for(tasks):
    async def main():
        later(0.1, lambda: tasks.insert({"n": 1}))
        later(0.2, lambda: tasks.insert({"n": 2}))
        got = []
        with tasks.watch() as live:
            async for event in live:
                got.append(event.doc["n"])
                if len(got) == 2:
                    break
        return got

    assert asyncio.run(main()) == [1, 2]


def test_async_for_leaves_nothing_behind_after_a_break(tasks):
    for n in range(5):
        tasks.insert({"n": n})

    async def main(live):
        firsts = []
        async for event in live:
            firsts.append(event.doc["n"])
            if len(firsts) == 2:
                break
        rest = []
        async for event in live:
            rest.append(event.doc["n"])
            if len(rest) == 2:
                break
        return firsts, rest

    with tasks.watch() as live:
        assert asyncio.run(main(live)) == ([0, 1], [2, 3])
        assert live.next(0).doc["n"] == 4


def test_an_async_wait_cancelled_after_its_event_came_keeps_it(db, tasks):
    async def main(live):
        waiting = asyncio.ensure_future(anext(aiter(live)))
        await asyncio.sleep(0.05)
        tasks.insert({"n": 1})
        # The loop blocked while the reader's thread takes the event and
        # hands it over: the hand-over waits for the loop, the cancel
        # comes first.
        time.sleep(0.3)
        # Meanwhile the feed's source ends, every event it sent read.
        db.close()
        assert live.next(0) is None and live.ended
        waiting.cancel()
        with pytest.raises(asyncio.CancelledError):
            await waiting

    with tasks.watch() as live:
        asyncio.run(main(live))
        # The event kept: the feed hasn't ended until it is read.
        assert not live.ended
        event = live.next(0)
        assert event is not None and event.doc["n"] == 1
        assert live.next(0) is None and live.ended


def keep_one(tasks, live):
    """An event read for an `async for` whose wait was then cancelled."""

    async def main():
        waiting = asyncio.ensure_future(anext(aiter(live)))
        await asyncio.sleep(0.05)
        tasks.insert({"n": 1})
        time.sleep(0.3)
        waiting.cancel()
        with pytest.raises(asyncio.CancelledError):
            await waiting

    asyncio.run(main())


def test_a_close_lets_go_of_a_kept_event(tasks):
    live = tasks.watch()
    keep_one(tasks, live)
    live.close()
    assert live.next(0) is None and live.ended


def test_an_event_that_comes_for_a_cancelled_wait_is_kept(tasks):
    # The wait is cancelled while the reader's thread waits in a slice;
    # the event comes within that slice, for a wait no one awaits.
    async def main(live):
        waiting = asyncio.ensure_future(anext(aiter(live)))
        await asyncio.sleep(0.05)
        waiting.cancel()
        with pytest.raises(asyncio.CancelledError):
            await waiting
        tasks.insert({"n": 1})

    with tasks.watch() as live:
        asyncio.run(main(live))
        event = live.next(1)
        assert event is not None and event.doc["n"] == 1


def test_async_for_stops_at_the_end(tmp_path):
    db = trunkdb.open(tmp_path / "a.trunkdb")
    tasks = db.collection("tasks")
    live = tasks.watch()
    tasks.insert({"n": 1})

    async def main():
        later(0.2, db.close)
        return [event.doc["n"] async for event in live]

    assert asyncio.run(main()) == [1]
    assert live.ended


def test_async_for_lets_the_loop_run(tasks):
    async def main(live):
        ticks = 0

        async def tick():
            nonlocal ticks
            while True:
                ticks += 1
                await asyncio.sleep(0.01)

        ticker = asyncio.ensure_future(tick())
        later(0.3, lambda: tasks.insert({"n": 1}))
        async for event in live:
            break
        ticker.cancel()
        return ticks, event

    with tasks.watch() as live:
        ticks, event = asyncio.run(main(live))
    assert event.doc["n"] == 1
    assert ticks >= 5


def test_a_read_only_open_beside_a_writer_reports_its_commits(tmp_path):
    path = tmp_path / "beside.trunkdb"
    writer = subprocess.Popen(
        [
            sys.executable,
            "-c",
            textwrap.dedent(
                """
                import sys, trunkdb
                with trunkdb.open(sys.argv[1]) as db:
                    tasks = db.collection("tasks")
                    tasks.insert({"n": 0})
                    print("ready", flush=True)
                    for line in sys.stdin:
                        tasks.insert({"n": int(line)})
                """
            ),
            str(path),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        assert writer.stdout.readline() == "ready\n"
        with trunkdb.open(path, read_only=True, watch_every=0.02) as reader:
            with reader.collection("tasks").watch() as live:
                assert live.next(5).doc["n"] == 0
                writer.stdin.write("7\n")
                writer.stdin.flush()
                event = live.next(10)
                assert event is not None and event.doc["n"] == 7
        # Looking every 30 seconds, it hasn't seen the next commit yet.
        with trunkdb.open(path, read_only=True, watch_every=30) as reader:
            with reader.collection("tasks").watch() as live:
                assert len([live.next(5), live.next(5)]) == 2
                writer.stdin.write("8\n")
                writer.stdin.flush()
                assert live.next(1) is None
    finally:
        writer.stdin.close()
        writer.wait(timeout=30)
        writer.stdout.close()


def test_watch_every_is_a_number_of_seconds(tmp_path):
    for wrong in (0, -1, float("inf")):
        with pytest.raises(ValueError, match="watch_every"):
            trunkdb.open(tmp_path / "w.trunkdb", watch_every=wrong)
