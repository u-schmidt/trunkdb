import pytest

import trunkdb


@pytest.fixture
def db(tmp_path):
    with trunkdb.open(tmp_path / "test.trunkdb") as db:
        yield db


@pytest.fixture
def tasks(db):
    return db.collection("tasks")
