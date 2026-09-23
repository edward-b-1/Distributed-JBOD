from typing import Any, Optional

BUILD: str

class ObjectInfo:
    key: str
    size: int
    version: str
    content_type: Optional[str]
    metadata: dict[str, str]
    created: str
    k: int
    m: int
    block_size: int

class KeyEntry:
    key: str
    size: int
    version: str

class ListPage:
    keys: list[KeyEntry]
    truncated: bool
    @property
    def next_start_after(self) -> Optional[str]: ...

class Status:
    cluster_id: str
    cluster_name: Optional[str]
    document_version: int
    coordinator: str
    nodes: list[dict[str, Any]]
    transport: str
    devices: list[dict[str, Any]]

class Identity:
    address: str
    cluster_id: str
    cluster_name: Optional[str]
    node: Optional[str]
    build: Optional[str]
    document_version: int

class Client:
    def __init__(
        self,
        nodes: list[str],
        cluster: Optional[str] = None,
        tls_ca: Optional[str] = None,
        tls_cert: Optional[str] = None,
        tls_key: Optional[str] = None,
    ) -> None: ...
    @property
    def cluster_id(self) -> str: ...
    @property
    def node_address(self) -> Optional[str]: ...
    def put(
        self,
        key: str,
        data: bytes,
        content_type: Optional[str] = None,
        metadata: Optional[dict[str, str]] = None,
    ) -> str: ...
    def put_file(
        self,
        key: str,
        path: str,
        content_type: Optional[str] = None,
        metadata: Optional[dict[str, str]] = None,
    ) -> str: ...
    def get(self, key: str) -> bytes: ...
    def get_to_file(self, key: str, path: str) -> ObjectInfo: ...
    def head(self, key: str) -> ObjectInfo: ...
    def delete(self, key: str) -> None: ...
    def list(
        self,
        prefix: Optional[str] = None,
        start_after: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> ListPage: ...
    def list_all(self, prefix: Optional[str] = None) -> list[KeyEntry]: ...
    def device_contents(self, device: str) -> dict[str, Any]: ...
    def repair(self, key: str) -> dict[str, Any]: ...
    def status(self) -> Status: ...
    def identity(self) -> Identity: ...
    def cluster_document(self) -> dict[str, Any]: ...
