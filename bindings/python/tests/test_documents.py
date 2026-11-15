"""Documents both ways (SPEC §197.4)."""

import datetime as dt
import pickle
from decimal import Decimal

import pytest

import trunkdb

UTC = dt.timezone.utc


def round_trip(tasks, doc):
    return tasks.get(tasks.insert(doc))


def test_every_type_comes_back_as_it_went_in(tasks):
    other = trunkdb.DocId("0192f0c4-0000-7000-8000-000000000001")
    doc = {
        "none": None,
        "yes": True,
        "n": -(2**63),
        "big": 2**63 - 1,
        "x": 2.5,
        "s": "text ✓",
        "raw": b"\x00\xff",
        "list": [1, "two", [3]],
        "obj": {"b": 1, "a": {"deep": False}},
        "ref": other,
        "when": dt.datetime(2026, 11, 15, 10, 30, 1, 123456, tzinfo=UTC),
        "day": dt.date(2026, 11, 20),
        "time": dt.time(8, 15, 30, 250),
    }
    back = round_trip(tasks, doc)
    assert isinstance(back.pop("_id"), trunkdb.DocId)
    assert back == doc
    assert type(back["yes"]) is bool
    assert type(back["ref"]) is trunkdb.DocId


def test_fields_keep_their_order(tasks):
    back = round_trip(tasks, {"z": 1, "a": 2, "m": {"y": 1, "b": 2}})
    assert list(back) == ["_id", "z", "a", "m"]
    assert list(back["m"]) == ["y", "b"]


def test_a_bool_is_never_an_int(tasks):
    tasks.insert({"flag": True, "n": 1})
    assert tasks.count("flag = true") == 1
    assert tasks.count("n = 1") == 1
    assert tasks.count("flag = 1") == 0


def test_a_tuple_is_a_list_and_bytes_like_are_bytes(tasks):
    back = round_trip(
        tasks,
        {"t": (1, 2), "ba": bytearray(b"ab"), "mv": memoryview(b"cd")},
    )
    assert back["t"] == [1, 2]
    assert back["ba"] == b"ab" and type(back["ba"]) is bytes
    assert back["mv"] == b"cd"


def test_an_int_outside_i64_overflows_naming_its_path(tasks):
    with pytest.raises(OverflowError, match=r"^counts\[1\]: 9223372036854775808 is outside"):
        tasks.insert({"counts": [1, 2**63]})
    assert tasks.count() == 0


def test_a_value_trunkdb_doesnt_store_is_a_type_error_naming_its_path(tasks):
    with pytest.raises(TypeError, match=r"^tags\[1\]\.price: a Decimal is not a value"):
        tasks.insert({"tags": [{}, {"price": Decimal("1.5")}]})
    with pytest.raises(TypeError, match=r"^s: a set is not"):
        tasks.insert({"s": {1, 2}})


def test_a_key_that_isnt_a_str_is_a_type_error(tasks):
    with pytest.raises(TypeError, match=r"^inner: a key that is a int, 1"):
        tasks.insert({"inner": {1: "one"}})


def test_a_document_is_a_dict(tasks):
    with pytest.raises(TypeError, match="a document is a dict, not a list"):
        tasks.insert([("a", 1)])


def test_a_datetime_without_a_zone_is_refused(tasks):
    with pytest.raises(ValueError, match=r"^due: a datetime without a tzinfo"):
        tasks.insert({"due": dt.datetime(2026, 11, 15, 9, 0)})


def test_a_datetime_in_any_zone_comes_back_in_utc(tasks):
    berlin = dt.timezone(dt.timedelta(hours=1))
    back = round_trip(tasks, {"at": dt.datetime(2026, 11, 15, 10, 30, tzinfo=berlin)})
    assert back["at"] == dt.datetime(2026, 11, 15, 9, 30, tzinfo=UTC)
    assert back["at"].tzinfo is UTC


def test_a_zone_west_of_utc_and_across_midnight(tasks):
    west = dt.timezone(dt.timedelta(hours=-5, minutes=-30))
    back = round_trip(tasks, {"at": dt.datetime(2026, 12, 31, 20, 0, tzinfo=west)})
    assert back["at"] == dt.datetime(2027, 1, 1, 1, 30, tzinfo=UTC)


def test_datetimes_compare_in_time_order_in_a_filter(tasks):
    tasks.insert({"at": dt.datetime(2026, 1, 1, tzinfo=UTC)})
    tasks.insert({"at": dt.datetime(2026, 6, 1, tzinfo=UTC)})
    since = dt.datetime(2026, 3, 1, 1, tzinfo=dt.timezone(dt.timedelta(hours=1)))
    assert tasks.count("at > :since", {"since": since}) == 1


def test_a_time_with_a_zone_is_refused(tasks):
    with pytest.raises(ValueError, match="a time with a tzinfo"):
        tasks.insert({"t": dt.time(8, 0, tzinfo=UTC)})


def test_nanoseconds_are_cut_to_microseconds(db, tasks):
    db.batch(
        '{"op": "insert", "collection": "tasks", '
        '"document": {"at": {"$date": "2026-11-15T10:00:00.123456789Z"}, '
        '"t": {"$time": "08:00:00.999999999"}}}'
    )
    found = tasks.find_one()
    assert found["at"] == dt.datetime(2026, 11, 15, 10, 0, 0, 123456, tzinfo=UTC)
    assert found["t"] == dt.time(8, 0, 0, 999999)


def test_a_date_python_cant_hold_is_a_value_error_naming_its_path(db, tasks):
    db.batch(
        '{"op": "insert", "collection": "tasks", '
        '"document": {"far": [{"$date": "+10000-01-01T00:00:00Z"}]}}'
    )
    with pytest.raises(ValueError, match=r"^\[0\]\.far\[0\]: \+10000-01-01T00:00:00Z is outside"):
        tasks.find()
    with pytest.raises(ValueError, match=r"^far\[0\]: "):
        tasks.find_one()


def test_doc_id_from_and_to_its_text():
    text = "0192f0c4-1234-7abc-8def-0123456789ab"
    made = trunkdb.DocId(text)
    assert str(made) == text
    assert repr(made) == f"DocId('{text}')"
    assert trunkdb.DocId(text.replace("-", "")) == made
    assert hash(made) == hash(trunkdb.DocId(text))
    assert pickle.loads(pickle.dumps(made)) == made
    with pytest.raises(ValueError, match="is not a document id"):
        trunkdb.DocId("nope")


def test_doc_ids_order_by_when_they_were_made(tasks):
    ids = [tasks.insert({"n": n}) for n in range(5)]
    assert sorted(reversed(ids)) == ids
    assert ids[0] < ids[1] and ids[4] > ids[3]
    assert len({*ids, *ids}) == 5


def test_a_given_id_is_kept_and_the_text_of_an_id_finds_it(tasks):
    given = trunkdb.DocId("0192f0c4-1234-7abc-8def-0123456789ab")
    assert tasks.insert({"_id": given, "n": 1}) == given
    assert tasks.get(str(given))["n"] == 1
    with pytest.raises(TypeError, match="an id is a DocId or its text"):
        tasks.get(42)


def nested(levels):
    """A document `levels` deep, the document itself the first."""
    inner = []
    for _ in range(levels - 2):
        inner = [inner]
    return {"a": inner}


def test_a_container_that_holds_itself_is_refused(tasks):
    doc = {"n": 1}
    doc["self"] = doc
    with pytest.raises(ValueError, match=r"^self: a dict that holds itself"):
        tasks.insert(doc)
    items = [1]
    items.append(items)
    with pytest.raises(ValueError, match=r"^tags\[1\]: a list that holds itself"):
        tasks.insert({"tags": items})
    with pytest.raises(ValueError, match=r"^:t\[1\]: a list that holds itself"):
        tasks.find("tags = :t", {"t": items})
    # The same value twice is no loop.
    shared = {"x": 1}
    id = tasks.insert({"a": shared, "b": [shared, shared]})
    assert tasks.get(id)["b"] == [{"x": 1}, {"x": 1}]


def test_a_document_nests_as_deep_as_trunkdb_allows(tasks):
    id = tasks.insert(nested(64))
    assert tasks.get(id)["a"] == nested(64)["a"]
    with pytest.raises(ValueError, match="nested deeper than a document may, 64 levels"):
        tasks.insert(nested(65))
    with pytest.raises(ValueError, match="nested deeper"):
        tasks.insert(nested(100_000))
