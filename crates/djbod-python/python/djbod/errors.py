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
