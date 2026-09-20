"""A node for the tests: `djbod-node` from DJBOD_NODE_BIN (or on PATH),
creating a cluster of one node with two directories as devices."""

import os
import shutil
import socket
import subprocess
import time
import uuid

import pytest


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_for(port: int, process: subprocess.Popen, seconds: float = 20.0) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"djbod-node exited with {process.returncode}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("djbod-node did not start listening")


@pytest.fixture(scope="session")
def node(tmp_path_factory):
    binary = os.environ.get("DJBOD_NODE_BIN") or shutil.which("djbod-node")
    if not binary:
        pytest.skip("set DJBOD_NODE_BIN to a built djbod-node")
    root = tmp_path_factory.mktemp("node")
    devices = [root / "d0", root / "d1"]
    for device in devices:
        device.mkdir()
    (root / "state").mkdir()
    port = _free_port()
    config = root / "node.toml"
    config.write_text(
        f'node_id = "{uuid.uuid4()}"\n'
        f'listen = "127.0.0.1:{port}"\n'
        f'state_dir = "{root / "state"}"\n'
        f"devices = [{', '.join(repr(str(d)) for d in devices)}]\n"
        "allow_shared_filesystem = true\n"
    )
    subprocess.run(
        [binary, "init-cluster", "--config", str(config), "--k", "1", "--m", "1", "--name", "pytest"],
        check=True,
        capture_output=True,
    )
    process = subprocess.Popen([binary, "run", "--config", str(config)], stderr=subprocess.DEVNULL)
    try:
        _wait_for(port, process)
        yield f"127.0.0.1:{port}"
    finally:
        process.terminate()
        process.wait(timeout=10)
