"""The database, snapshots, batches and reports (SPEC §197.3), closing
(§197.6), threads (§197.7) and the errors of files (§197.5)."""

import errno
import gc
import subprocess
import sys
import textwrap
import threading
import time

import pytest

import trunkdb


def test_version():
    assert trunkdb.version() == trunkdb.__version__
    assert trunkdb.__version__.count(".") == 2


def test_open_takes_a_path_and_options(tmp_path):
    path = tmp_path / "options.trunkdb"
    with trunkdb.open(
        path,
        cache_size=64,
        snapshot_memory=1 << 20,
        checkpoint_pages=100,
        checkpoint_wal_bytes=1 << 20,
    ) as db:
        db.collection("t").insert({"a": 1})
    with trunkdb.open(str(path)) as db:
        assert db.collection("t").count() == 1


def test_collections_and_drop_collection(db):
    db.collection("b").insert({})
    db.collection("a").insert({})
    assert db.collections() == ["a", "b"]
    assert db.drop_collection("a")
    assert not db.drop_collection("a")
    assert db.collections() == ["b"]


def test_a_read_only_open_refuses_writes(tmp_path):
    path = tmp_path / "ro.trunkdb"
    with trunkdb.open(path) as db:
        db.collection("t").insert({"a": 1})
    with trunkdb.open(path, read_only=True) as db:
        assert db.collection("t").count() == 1
        with pytest.raises(trunkdb.ReadOnlyError) as caught:
            db.collection("t").insert({"a": 2})
        assert caught.value.kind == "read_only"


def test_a_second_writer_is_a_blocking_io_error(tmp_path):
    path = tmp_path / "busy.trunkdb"
    with trunkdb.open(path):
        with pytest.raises(BlockingIOError) as caught:
            trunkdb.open(path)
    assert caught.value.errno == errno.EAGAIN
    assert caught.value.filename == str(path)


def test_a_file_that_isnt_trunkdbs_is_a_format_error(tmp_path):
    path = tmp_path / "other.trunkdb"
    path.write_bytes(b"not a database, " * 300)
    with pytest.raises(trunkdb.FormatError):
        trunkdb.open(path)


def test_a_missing_file_is_pythons_own_error(db, tmp_path):
    missing = tmp_path / "nowhere" / "x.trunkdb"
    with pytest.raises(FileNotFoundError) as caught:
        trunkdb.open(missing)
    assert caught.value.filename == str(missing)
    with pytest.raises(FileNotFoundError):
        db.import_file(tmp_path / "missing.jsonl")


def test_a_snapshot_reads_the_database_as_it_was(db, tasks):
    tasks.insert({"n": 1})
    with db.snapshot() as then:
        tasks.insert({"n": 2})
        view = then.collection("tasks")
        assert view.count() == 1
        assert [d["n"] for d in view.find()] == [1]
        assert view.find_one("n = 2") is None
        assert view.get(tasks.find_one("n = 2")["_id"]) is None
        assert then.collections() == ["tasks"]
        assert view.aggregate("", "count() as n") == [{"n": 1}]
        assert "n" in view.explain("n = 1") or view.explain("n = 1")
        assert view.indexes() == []
        with view.cursor() as cursor:
            assert len(list(cursor)) == 1
        assert view.name == "tasks"
        assert repr(view) == "<trunkdb.View 'tasks'>"
    assert then.closed
    with pytest.raises(ValueError, match="closed snapshot"):
        view.count()
    assert tasks.count() == 2


def test_compact_fails_while_a_snapshot_is_open(db, tasks):
    # Pages freed, so that compacting would rewrite the file.
    tasks.insert_many([{"n": n, "text": "x" * 2000} for n in range(300)])
    tasks.delete_many("")
    db.checkpoint()
    with db.snapshot():
        with pytest.raises(trunkdb.Error) as caught:
            db.compact()
        assert caught.value.kind == "snapshot_open"
    assert set(db.compact()) == {"pages_before", "pages_after"}


def test_a_batch_of_op_dicts(db, tasks):
    tasks.insert({"name": "ada", "n": 1})
    written = db.batch(
        [
            {"op": "insert", "collection": "tasks", "document": {"name": "bo"}},
            {"op": "update", "collection": "tasks", "filter": "name = :who",
             "update": {"$inc": {"n": 1}}},
            {"op": "upsert", "collection": "visits", "filter": 'page = "/"',
             "update": {"$inc": {"hits": 1}}},
        ],
        {"who": "ada"},
    )
    ops = written["ops"]
    assert [op["op"] for op in ops] == ["insert", "update", "upsert"]
    assert [op["line"] for op in ops] == [1, 2, 3]
    assert tasks.get(ops[0]["id"])["name"] == "bo"
    assert isinstance(ops[2]["id"], trunkdb.DocId) and ops[2]["inserted"] is True
    assert written["total"] == 3 and written["dry_run"] is False
    assert tasks.find_one('name = "ada"')["n"] == 2


def test_a_batch_as_json_lines_and_a_dry_run(db, tasks):
    written = db.batch(
        '{"op": "insert", "collection": "tasks", "document": {"n": 1}}\n\n'
        '{"op": "delete", "collection": "tasks", "filter": "", "all": true}',
        dry_run=True,
    )
    assert [op["line"] for op in written["ops"]] == [1, 3]
    assert "id" not in written["ops"][0]
    assert written["dry_run"] is True
    assert tasks.count() == 0


def test_a_failed_precondition_names_its_line(db, tasks):
    tasks.insert({"name": "ada"})
    with pytest.raises(trunkdb.PreconditionFailedError) as caught:
        db.batch(
            [
                {"op": "insert", "collection": "tasks", "document": {"name": "bo"}},
                {"op": "require_none", "collection": "tasks", "filter": 'name = "ada"'},
            ]
        )
    error = caught.value
    assert (error.line, error.op, error.matched, error.collection) == (2, 1, True, "tasks")
    assert tasks.count() == 1


def test_a_batch_line_that_doesnt_read_names_its_line(db):
    with pytest.raises(trunkdb.ParseError) as caught:
        db.batch([{"op": "insert", "collection": "t", "document": {}},
                  {"op": "delete", "collection": "t", "filter": "n = = 1"}])
    assert caught.value.line == 2
    assert caught.value.text == "n = = 1"
    assert caught.value.offset == 4
    with pytest.raises(trunkdb.ParseError) as caught:
        db.batch([{"op": "fly", "collection": "t"}])
    assert caught.value.line == 1
    with pytest.raises(trunkdb.ParseError) as caught:
        db.batch([{"op": "delete", "collection": "t", "filter": "n = :n"}], {"n": 1, "x": 2})
    assert caught.value.parameter == "x"
    with pytest.raises(TypeError, match=r"^ops\[0\]: a document is a dict"):
        db.batch(["insert"])


def test_reports_are_dicts(db, tasks):
    tasks.insert({"n": 1})
    tasks.ensure_index("n", unique=True)
    db.checkpoint()
    checked = db.check()
    assert checked["ok"] is True and checked["documents"] == 1
    assert checked["problems"] == []
    stats = db.stats()
    assert set(stats) >= {"file", "pages", "collections"}
    assert tasks.indexes() == [{"fields": ["n"], "unique": True, "sparse": False}]


def test_export_and_import(db, tasks, tmp_path):
    tasks.insert_many([{"n": n} for n in range(3)])
    summary = db.export(tmp_path / "out.jsonl")
    assert summary == {"collections": 1, "documents": 3, "ended": True}
    with trunkdb.open(tmp_path / "copy.trunkdb") as copy:
        assert copy.import_file(tmp_path / "out.jsonl")["documents"] == 3
        assert copy.collection("tasks").count() == 3


def test_a_closed_database_refuses_every_call(tmp_path):
    db = trunkdb.open(tmp_path / "c.trunkdb")
    tasks = db.collection("tasks")
    tasks.insert({"n": 1})
    snapshot = db.snapshot()
    view = snapshot.collection("tasks")
    cursor = tasks.cursor()
    db.close()
    db.close()
    assert db.closed and snapshot.closed and cursor.closed
    for call in (
        db.collections,
        db.checkpoint,
        tasks.count,
        lambda: tasks.insert({}),
        view.count,
        db.snapshot,
        lambda: next(cursor),
    ):
        with pytest.raises(ValueError, match="^closed database$"):
            call()
    assert repr(db).startswith("<trunkdb.Database closed '")


def test_a_database_closes_once_the_calls_under_way_return(tmp_path):
    db = trunkdb.open(tmp_path / "w.trunkdb")
    tasks = db.collection("tasks")
    tasks.insert_many([{"n": n} for n in range(2000)])
    started = threading.Event()
    found = []

    def read():
        started.set()
        found.append(len(tasks.find()))

    reader = threading.Thread(target=read)
    reader.start()
    started.wait()
    db.close()
    reader.join()
    assert found in ([2000], [])
    with trunkdb.open(tmp_path / "w.trunkdb") as again:
        assert again.collection("tasks").count() == 2000


def test_a_close_while_another_thread_makes_snapshots_and_cursors(tmp_path):
    # A snapshot or cursor made while the database closes, outside the
    # GIL, is closed with it: none holds the file after.
    path = tmp_path / "race.trunkdb"
    with trunkdb.open(path) as db:
        db.collection("t").insert_many([{"n": n} for n in range(200)])
    for _ in range(20):
        db = trunkdb.open(path)
        tasks = db.collection("t")
        made = []
        started = threading.Event()

        def take():
            started.set()
            while True:
                try:
                    made.append(db.snapshot())
                    made.append(tasks.cursor())
                except ValueError:
                    return

        taker = threading.Thread(target=take)
        taker.start()
        started.wait()
        time.sleep(0.002)
        db.close()
        taker.join()
        assert made and all(m.closed for m in made)
        with trunkdb.open(path):
            pass


def test_unclosed_things_warn_as_a_file_does(tmp_path):
    with pytest.warns(ResourceWarning, match="unclosed database"):
        db = trunkdb.open(tmp_path / "u.trunkdb")
        del db
        gc.collect()
    with trunkdb.open(tmp_path / "u.trunkdb") as db:
        db.collection("t").insert({"n": 1})
        db.collection("t").insert({"n": 2})
        with pytest.warns(ResourceWarning, match="unclosed snapshot"):
            snapshot = db.snapshot()
            del snapshot
            gc.collect()
        with pytest.warns(ResourceWarning, match="unclosed cursor"):
            cursor = db.collection("t").cursor()
            next(cursor)
            del cursor
            gc.collect()
        # A cursor read to its end closed itself: nothing to warn of.
        assert len(list(db.collection("t").cursor())) == 2


def test_an_exception_leaving_a_cursors_for_comes_through_whole(tmp_path):
    # A cursor freed while an exception is on its way warns without
    # taking the exception's place, which crashed the interpreter.
    script = textwrap.dedent(
        f"""
        import trunkdb
        with trunkdb.open({str(tmp_path / "x.trunkdb")!r}) as db:
            tasks = db.collection("tasks")
            tasks.insert({{"a": 1}})
            tasks.insert({{"a": 2}})
            for doc in tasks.cursor():
                doc["missing"]
        """
    )
    done = subprocess.run(
        [sys.executable, "-W", "always::ResourceWarning", "-c", script],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 1, done.stderr
    assert "KeyError: 'missing'" in done.stderr
    assert "ResourceWarning: unclosed cursor" in done.stderr


def test_one_database_from_many_threads(db, tasks):
    def write(thread):
        for n in range(50):
            tasks.insert({"thread": thread, "n": n})

    threads = [threading.Thread(target=write, args=(t,)) for t in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert tasks.count() == 200
    assert tasks.aggregate("", "count() as n group by thread") == [
        {"thread": t, "n": 50} for t in range(4)
    ]


def test_the_gil_is_let_go_while_trunkdb_works(tasks):
    # While one call reads, another thread runs Python: it notes the
    # time each millisecond, and some of those times fall inside the
    # call, well away from its ends, where Python could switch threads
    # on its own. A release build reads fast: documents are added until
    # the call takes long enough to tell.
    for _ in range(6):
        tasks.insert_many([{"text": "x" * 500} for _ in range(40000)])
        start = time.perf_counter()
        tasks.count("text = :t", {"t": "y"})
        if time.perf_counter() - start > 0.08:
            break
    stop = threading.Event()
    ticks = []

    def tick():
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.001)

    ticker = threading.Thread(target=tick)
    ticker.start()
    try:
        time.sleep(0.01)
        start = time.perf_counter()
        tasks.count("text = :t", {"t": "y"})
        end = time.perf_counter()
    finally:
        stop.set()
        ticker.join()
    margin = (end - start) / 4
    assert end - start > 0.02, "the call is too short to tell"
    inside = [t for t in ticks if start + margin < t < end - margin]
    assert inside
