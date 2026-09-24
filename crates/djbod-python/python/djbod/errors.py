"""The exceptions the client raises. Defined in Python so they subclass
naturally and carry the node's error detail (SPEC 16.2) as attributes."""


class Error(Exception):
    """Any failure of a djbod call."""


class Unreachable(Error):
    """No node could be reached. `attempts` lists each address and why."""

    def __init__(self, message: str, attempts: list[tuple[str, str]]):
        super().__init__(message)
        self.attempts = attempts


class NodeError(Error):
    """A node answered with an error. `code` is its name as `djbod --json`
    spells it, for example `not_found` or `node_unreachable`; `detail` is
    the whole error as the node sent it: code, message, and whichever of
    node, device, key, version, shard_index and stripe apply."""

    def __init__(self, message: str, detail: dict):
        super().__init__(message)
        self.detail = detail
        self.code = detail.get("code")


class NotFound(NodeError):
    """No object under that key."""


class DegradedWrite(Warning):
    """A write was placed around devices the cluster cannot read (SPEC
    5.6). The object is stored, on the other devices; `key` and `version`
    name it, and `unavailable` lists each device it went around as a dict:
    device, node. The cluster needs attention, not the object."""

    def __init__(self, message: str, key: str, version: str, unavailable: list):
        super().__init__(message)
        self.key = key
        self.version = version
        self.unavailable = unavailable


class DegradedRead(Warning):
    """A read returned correct data only by reconstructing blocks from
    parity (SPEC 11.4). `key` names the object; `reconstructed` lists each
    block as a dict: stripe, shard_index, device, fault. The damage on disk
    stays until `repair` runs, and every read of the object pays again."""

    def __init__(self, message: str, key: str, reconstructed: list):
        super().__init__(message)
        self.key = key
        self.reconstructed = reconstructed
