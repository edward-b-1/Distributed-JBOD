# djbod for Python

The Distributed-JBOD client as a Python package: the Rust client library
(`djbod-client`) wrapped with PyO3, so paging, error detail, TLS and
failover between nodes exist once and behave as they do in `djbod`.

```python
import djbod

client = djbod.Client(["10.0.0.1:5263", "10.0.0.2:5263"])   # the cluster id is learned
client.put("photos/cat.jpg", open("cat.jpg", "rb").read(), content_type="image/jpeg")
data = client.get("photos/cat.jpg")
info = client.head("photos/cat.jpg")          # info.size, info.version, info.content_type
for entry in client.list_all(prefix="photos/"):
    print(entry.key, entry.size)
client.delete("photos/cat.jpg")

try:
    client.head("missing")
except djbod.NotFound as e:
    print(e.code, e.detail)                   # the node's error, as in `djbod --json`
```

Every method blocks and releases the interpreter lock while it waits on
the network. `put_file` and `get_to_file` stream, holding one chunk in
memory. `Client(..., cluster="<uuid>")` names the cluster when it is
known; `tls_ca`, `tls_cert` and `tls_key` take the same PEM files as the
`djbod` command.

## Installing

The package is not on PyPI yet, so `pip install djbod` does not install
it (and would install whatever else claims that name). Each
[GitHub release](https://github.com/edward-b-1/Distributed-JBOD/releases)
carries it as a wheel for Linux on x86_64 and on aarch64, for any glibc
from 2.17 and every Python from 3.10, named for example
`djbod-0.2.23-cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64.whl`.
Install the one for your machine by its URL, into a virtual environment:

```sh
v=0.2.23
arch=$(uname -m)        # x86_64 or aarch64
python3 -m venv .venv
.venv/bin/pip install "https://github.com/edward-b-1/Distributed-JBOD/releases/download/v$v/djbod-$v-cp310-abi3-manylinux_2_17_$arch.manylinux2014_$arch.whl"
.venv/bin/python -c 'import djbod; print(djbod.__version__)'
```

`uv pip install` takes the same URL. To pin it in a project, name it in
`requirements.txt`, or in `pyproject.toml`'s dependencies:

```
djbod @ https://github.com/edward-b-1/Distributed-JBOD/releases/download/v0.2.23/djbod-0.2.23-cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64.whl
```

To check the file first, download it and the release's `SHA256SUMS`,
with the GitHub CLI or from the release page, and compare:

```sh
gh release download v$v --repo edward-b-1/Distributed-JBOD --pattern "djbod-$v-*$arch.whl" --pattern SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
.venv/bin/pip install ./djbod-$v-*$arch.whl
```

Use the release that matches your nodes: a new minor version may not
work with the one before (SPEC 20.7.1), and its release notes say so. On macOS, Windows, musl-based
Linux, or any release before 0.2.23, build it from the source as below.

## Building

The package is built with [maturin](https://www.maturin.rs/) from this
directory; `uv` is the easiest way to get a Python with everything needed:

```sh
uv venv .venv && uv pip install --python .venv maturin pytest
.venv/bin/maturin develop            # builds and installs into .venv
DJBOD_NODE_BIN=../../target/debug/djbod-node .venv/bin/pytest
```

`scripts/python-tests.sh` at the repository root does all of that. A
wheel for distribution is `maturin build --release`; it is an `abi3`
wheel, so one build serves every Python from 3.10 up.
