"""Client for Distributed-JBOD.

    import djbod
    client = djbod.Client(["10.0.0.1:5263"])
    client.put("key", b"bytes")
    client.get("key")

Every call blocks and releases the interpreter lock while waiting on the
network. Errors are `djbod.Error` and its subclasses: `djbod.NodeError`
when a node refused, with `.code` and `.detail` as the node reported
them, `djbod.NotFound` for the common case, and `djbod.Unreachable` when
no node could be reached.
"""

from ._native import (
    BUILD,
    Client,
    Identity,
    KeyEntry,
    ListPage,
    ObjectInfo,
    Status,
)
from .errors import DegradedRead, DegradedWrite, Error, NodeError, NotFound, Unreachable

__version__ = BUILD.split("+", 1)[0]

__all__ = [
    "DegradedRead",
    "DegradedWrite",
    "BUILD",
    "Client",
    "Error",
    "Identity",
    "KeyEntry",
    "ListPage",
    "NodeError",
    "NotFound",
    "ObjectInfo",
    "Status",
    "Unreachable",
]
