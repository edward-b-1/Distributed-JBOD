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
