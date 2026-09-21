import os

import pytest

import djbod


@pytest.fixture
def client(node):
    return djbod.Client([node])


def test_the_cluster_id_is_learned_and_the_node_identified(node):
    client = djbod.Client(["127.0.0.1:1", node])  # a dead address first
    assert len(client.cluster_id) == 36
    assert client.node_address == node
    identity = client.identity()
    assert identity.cluster_name == "pytest"
    assert identity.build == djbod.BUILD
    # Named explicitly, the same cluster is accepted.
    assert djbod.Client([node], cluster=client.cluster_id).status().cluster_name == "pytest"
    assert repr(client).startswith("Client(")


def test_objects_round_trip(client, tmp_path):
    body = bytes(range(256)) * 300
    version = client.put("photos/cat.jpg", body, content_type="image/jpeg", metadata={"camera": "x100"})
    assert isinstance(version, str) and len(version) == 26
    assert client.get("photos/cat.jpg") == body
    info = client.head("photos/cat.jpg")
    assert (info.key, info.size, info.version) == ("photos/cat.jpg", len(body), version)
    assert info.content_type == "image/jpeg"
    assert info.metadata == {"camera": "x100"}
    assert info.k == 1 and info.m == 1
    assert "T" in info.created

    source = tmp_path / "big.bin"
    source.write_bytes(b"\x07" * (3 * 64 * 1024 + 11))
    client.put_file("big", str(source))
    target = tmp_path / "copy.bin"
    fetched = client.get_to_file("big", str(target))
    assert target.read_bytes() == source.read_bytes()
    assert fetched.size == source.stat().st_size

    keys = client.list_all(prefix="photos/")
    assert [k.key for k in keys] == ["photos/cat.jpg"]
    assert keys[0].size == len(body)
    page = client.list(limit=1)
    assert len(page.keys) == 1 and page.truncated and page.next_start_after == page.keys[0].key
    rest = client.list(start_after=page.next_start_after)
    assert not rest.truncated
    assert [k.key for k in page.keys + rest.keys] == ["big", "photos/cat.jpg"]

    report = client.repair("big")
    assert report["key"] == "big" and len(report["shards"]) == 2

    client.delete("big")
    client.delete("photos/cat.jpg")
    assert client.list_all() == []


def test_errors_are_python_exceptions(client, node):
    with pytest.raises(djbod.NotFound) as caught:
        client.head("nothing")
    assert caught.value.code == "not_found"  # as `djbod --json` spells it
    assert caught.value.detail["key"] == "nothing"
    assert isinstance(caught.value, djbod.NodeError)
    assert isinstance(caught.value, djbod.Error)

    with pytest.raises(djbod.Unreachable) as caught:
        djbod.Client(["127.0.0.1:1"])
    assert caught.value.attempts[0][0] == "127.0.0.1:1"

    with pytest.raises(ValueError):
        djbod.Client(["not an address"])
    with pytest.raises(ValueError):
        djbod.Client([node], cluster="not a uuid")


def test_status_and_document(client):
    status = client.status()
    assert status.cluster_name == "pytest"
    assert status.transport == "plain"
    assert len(status.devices) == 2
    assert status.devices[0]["state"] == "active"
    document = client.cluster_document()
    assert document["name"] == "pytest"
    assert document["version"] == status.document_version
    assert os.environ.get("DJBOD_NODE_BIN") or True


def test_device_contents_are_counted(client):
    status = client.status()
    device = status.devices[0]["device"]
    before = client.device_contents(device)
    client.put("counted", b"y" * 1000)
    after = client.device_contents(device)
    assert after["versions"] == before["versions"] + 1
    assert after["keys"] == before["keys"] + 1
    assert after["shard_bytes"] > before["shard_bytes"]
    assert after["device"] == device
    with pytest.raises(ValueError):
        client.device_contents("not a uuid")
    client.delete("counted")
