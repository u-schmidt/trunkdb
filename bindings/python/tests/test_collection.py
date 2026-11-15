"""A collection's reads and writes (SPEC §197.3) and their errors (§197.5)."""

import datetime as dt

import pytest

import trunkdb


@pytest.fixture
def team(tasks):
    for name, status, n in [("ada", "open", 3), ("bo", "done", 1), ("cy", "open", 2)]:
        tasks.insert({"name": name, "status": status, "n": n})
    return tasks


def names(docs):
    return [d["name"] for d in docs]


def test_find_with_a_filter_and_params(team):
    found = team.find("status = :s order by n", {"s": "open"})
    assert names(found) == ["cy", "ada"]
    assert len(team.find()) == 3
    assert team.find('name = "nobody"') == []


def test_find_one_count_and_get(team):
    assert team.find_one("n > 2")["name"] == "ada"
    assert team.find_one("n > 5") is None
    assert team.count('status = "open"') == 2
    first = team.find_one('name = "bo"')
    assert team.get(first["_id"]) == first
    assert team.get(trunkdb.DocId("0192f0c4-0000-7000-8000-000000000001")) is None


def test_a_value_no_text_uses_is_refused(team):
    with pytest.raises(trunkdb.ParseError, match=":stray") as caught:
        team.find("status = :s", {"s": "open", "stray": 1})
    assert caught.value.parameter == "stray"
    assert caught.value.kind == "parse"


def test_a_parameter_without_a_value_is_refused(team):
    with pytest.raises(trunkdb.ParseError) as caught:
        team.count("status = :s")
    assert caught.value.parameter == "s"


def test_a_filter_that_doesnt_read_names_where(team):
    with pytest.raises(trunkdb.ParseError) as caught:
        team.find("status = = 1")
    error = caught.value
    assert error.what == "filter"
    assert error.text == "status = = 1"
    assert error.offset == 9
    assert error.line is None
    assert isinstance(error, trunkdb.Error)


def test_a_param_value_is_checked_as_a_documents_is(team):
    with pytest.raises(ValueError, match=r"^:since: a datetime without a tzinfo"):
        team.find("at > :since", {"since": dt.datetime(2026, 1, 1)})
    with pytest.raises(TypeError, match="params is a dict"):
        team.find("n = :n", [("n", 1)])


def test_a_cursor_hands_out_each_and_closes_at_its_end(team):
    cursor = team.cursor("status = :s order by name", {"s": "open"})
    assert iter(cursor) is cursor
    assert names(cursor) == ["ada", "cy"]
    assert cursor.closed
    with pytest.raises(StopIteration):
        next(cursor)


def test_a_cursor_reads_the_collection_as_it_was_when_opened(team):
    with team.cursor() as cursor:
        team.insert({"name": "late", "status": "open", "n": 9})
        assert len(list(cursor)) == 3


def test_a_cursor_left_in_a_with_is_closed(team):
    with team.cursor() as cursor:
        next(cursor)
    assert cursor.closed
    with pytest.raises(ValueError, match="closed cursor"):
        next(cursor)


def test_aggregate_each_text_its_own_params(team):
    rows = team.aggregate(
        "status = :s", "count() as n, sum(n) as total group by status", {"s": "open"}
    )
    assert rows == [{"status": "open", "n": 2, "total": 5}]
    assert team.aggregate("", "count() as n") == [{"n": 3}]
    rows = team.aggregate(
        "n > :min", "sum(n) as total group by status having total > :over",
        {"min": 1, "over": 2},
    )
    assert rows == [{"status": "open", "total": 5}]
    with pytest.raises(trunkdb.ParseError, match=":stray"):
        team.aggregate("", "count() as n", {"stray": 1})


def test_explain_says_how(team):
    team.ensure_index("status")
    said = team.explain("status = :s", {"s": "open"})
    assert isinstance(said, str) and "status" in said


def test_insert_many_is_all_or_none(tasks):
    ids = tasks.insert_many(({"n": n} for n in range(3)))
    assert [tasks.get(i)["n"] for i in ids] == [0, 1, 2]
    with pytest.raises(trunkdb.DuplicateIdError) as caught:
        tasks.insert_many([{"n": 3}, {"_id": ids[0], "n": 4}])
    assert caught.value.id == ids[0]
    assert caught.value.collection == "tasks"
    assert tasks.count() == 3
    assert tasks.insert_many([]) == []
    with pytest.raises(TypeError, match=r"^\[1\]\.x: a set"):
        tasks.insert_many([{"n": 1}, {"x": set()}])


def test_update_replaces_the_whole_document(team):
    ada = team.find_one('name = "ada"')
    assert team.update(ada["_id"], {"name": "ada", "retired": True})
    assert team.get(ada["_id"]) == {"_id": ada["_id"], "name": "ada", "retired": True}
    missing = trunkdb.DocId("0192f0c4-0000-7000-8000-000000000001")
    assert not team.update(missing, {"name": "nobody"})
    assert team.get(missing) is None


def test_update_many_with_operators(team):
    changed = team.update_many(
        "status = :s", {"$inc": {"n": 10}, "$set": {"seen": True}}, {"s": "open"}
    )
    assert changed == 2
    assert sorted(d["n"] for d in team.find("seen = true")) == [12, 13]


def test_an_operator_that_cant_apply_is_an_update_error(team):
    with pytest.raises(trunkdb.UpdateError) as caught:
        team.update_many('name = "bo"', {"$inc": {"name": 1}})
    assert caught.value.collection == "tasks"
    assert isinstance(caught.value.id, trunkdb.DocId)
    assert caught.value.message


def test_update_operators_that_dont_read_are_a_parse_error(team):
    with pytest.raises(trunkdb.ParseError, match="update"):
        team.update_many("", {"$nope": {"n": 1}})
    with pytest.raises(trunkdb.ParseError, match="a dict"):
        team.update_many("", "$set n = 1")


def test_delete_and_delete_many(team):
    bo = team.find_one('name = "bo"')
    assert team.delete(bo["_id"])
    assert not team.delete(bo["_id"])
    assert team.delete_many("status = :s", {"s": "open"}) == 2
    assert team.count() == 0


def test_upsert_inserts_then_updates(tasks):
    first = tasks.upsert("page = :p", {"$inc": {"hits": 1}}, {"p": "/home"})
    assert first.inserted
    again = tasks.upsert("page = :p", {"$inc": {"hits": 1}}, {"p": "/home"})
    assert not again.inserted and again.id == first.id
    assert first != again
    assert tasks.get(first.id)["hits"] == 2
    assert "inserted=False" in repr(again)


def test_an_upsert_matching_several_fails(team):
    with pytest.raises(trunkdb.MultipleMatchesError) as caught:
        team.upsert('status = "open"', {"$set": {"x": 1}})
    assert caught.value.count == 2


def test_indexes(team):
    assert team.ensure_index("status")
    assert not team.ensure_index("status")
    assert team.ensure_index(["status", "n"], sparse=True)
    assert [i["fields"] for i in team.indexes()] == [["status"], ["status", "n"]]
    assert team.indexes()[1]["sparse"] is True
    assert team.drop_index(["status", "n"])
    assert not team.drop_index("nope")
    with pytest.raises(TypeError, match="an index's fields"):
        team.ensure_index(3)


def test_a_unique_index_refuses_a_second_value(team):
    team.ensure_index("name", unique=True)
    ada = team.find_one('name = "ada"')
    with pytest.raises(trunkdb.DuplicateValueError) as caught:
        team.insert({"name": "ada"})
    error = caught.value
    assert error.field == "name"
    assert error.existing == ada["_id"]
    assert isinstance(error.id, trunkdb.DocId)
    assert error.kind == "duplicate_value"


def test_drop(db, team):
    assert team.drop()
    assert not team.drop()
    assert db.collections() == []
    assert team.name == "tasks"
    assert repr(team) == "<trunkdb.Collection 'tasks'>"
