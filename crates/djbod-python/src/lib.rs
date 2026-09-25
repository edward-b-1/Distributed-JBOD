//! `djbod._native`: the Rust client (`djbod_client::blocking`) as a Python
//! extension module (SPEC 20.8.1). Every method releases the interpreter
//! lock while it waits on the network. The exception classes live in
//! `djbod.errors`, in Python, so they subclass naturally; this module
//! raises them by name.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Mutex;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pythonize::pythonize;
use uuid::Uuid;

use djbod_client::blocking::Client as Inner;
use djbod_client::transport::Connector;
use djbod_client::{ClientError, ClientOptions};
use djbod_core::record::{DeviceId, MetadataRecord};
use djbod_proto::message::{
    ErrorCode, KeyEntry as ProtoKeyEntry, ListQuery, MissingRecordCopy, ObjectRead, ObjectWrite,
    Reconstruction,
};

/// An object's record without its body.
#[pyclass(frozen, get_all)]
struct ObjectInfo {
    key: String,
    size: u64,
    version: String,
    content_type: Option<String>,
    metadata: BTreeMap<String, String>,
    /// When the version was written, RFC 3339.
    created: String,
    k: u8,
    m: u8,
    block_size: u64,
    /// What the read reconstructed from parity (SPEC 11.4), each entry a
    /// shard_index, device, fault, first_stripe and stripes; empty when
    /// none, and for `head`. The data was correct; nothing was repaired.
    reconstructed: Py<PyAny>,
    /// The record copies the lookup went without (SPEC 9.4.4), each entry
    /// a device and a fault of kind `missing`, `stale` or `unavailable`;
    /// empty when every device had one. The record was trusted on the
    /// copies that agreed; nothing was repaired.
    missing_records: Py<PyAny>,
}

impl ObjectInfo {
    fn from_read(py: Python<'_>, read: ObjectRead) -> PyResult<ObjectInfo> {
        let mut info = ObjectInfo::from_record(py, read.record)?;
        info.reconstructed = pythonize(py, &read.reconstructed)?.unbind();
        info.missing_records = pythonize(py, &read.missing_records)?.unbind();
        Ok(info)
    }

    fn from_record(py: Python<'_>, record: MetadataRecord) -> PyResult<ObjectInfo> {
        Ok(ObjectInfo {
            key: record.key,
            size: record.size,
            version: record.version.to_text(),
            content_type: record.content_type,
            metadata: record.user_metadata,
            created: record
                .created
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| record.created.to_string()),
            k: record.k,
            m: record.m,
            block_size: record.block_size,
            reconstructed: pythonize(py, &Vec::<Reconstruction>::new())?.unbind(),
            missing_records: pythonize(py, &Vec::<MissingRecordCopy>::new())?.unbind(),
        })
    }
}

/// A `DegradedWrite` warning (SPEC 5.6) when a write went around devices
/// the cluster cannot read: the object is safe, the cluster is not whole.
fn warn_if_placed_around(py: Python<'_>, key: &str, write: &ObjectWrite) -> PyResult<()> {
    if write.unavailable.is_empty() {
        return Ok(());
    }
    let message = format!(
        "{key}: placed around {} unavailable device(s); the object is stored on the others, and the cluster needs attention",
        write.unavailable.len()
    );
    let warning = py
        .import("djbod.errors")?
        .getattr("DegradedWrite")?
        .call1((
            message,
            key,
            write.version.to_text(),
            pythonize(py, &write.unavailable)?,
        ))?;
    py.import("warnings")?.call_method1("warn", (warning,))?;
    Ok(())
}

/// A `DegradedRead` warning (SPEC 11.4, 9.4.4) when a read had to
/// reconstruct or went without a record copy, so a notebook sees it once
/// per object without the call failing.
fn warn_if_degraded(py: Python<'_>, key: &str, read: &ObjectRead) -> PyResult<()> {
    if read.reconstructed.is_empty() && read.missing_records.is_empty() {
        return Ok(());
    }
    let mut notes = Vec::new();
    if !read.reconstructed.is_empty() {
        notes.push(format!(
            "{} block(s) reconstructed from parity; the data is correct, the damage on disk is not repaired, and every read pays again until repair runs",
            read.reconstructed.len()
        ));
    }
    if !read.missing_records.is_empty() {
        notes.push(format!(
            "{} record copy(ies) could not be read; the record was trusted on the copies that agree, and repair rewrites the missing ones",
            read.missing_records.len()
        ));
    }
    let message = format!("{key}: {}", notes.join("; "));
    let warning = py.import("djbod.errors")?.getattr("DegradedRead")?.call1((
        message,
        key,
        pythonize(py, &read.reconstructed)?,
        pythonize(py, &read.missing_records)?,
    ))?;
    py.import("warnings")?.call_method1("warn", (warning,))?;
    Ok(())
}

#[pymethods]
impl ObjectInfo {
    fn __repr__(&self) -> String {
        format!(
            "ObjectInfo(key={:?}, size={}, version={:?})",
            self.key, self.size, self.version
        )
    }
}

/// One line of a listing.
#[pyclass(frozen, get_all)]
struct KeyEntry {
    key: String,
    size: u64,
    version: String,
}

impl KeyEntry {
    fn from_proto(entry: ProtoKeyEntry) -> KeyEntry {
        KeyEntry {
            key: entry.key,
            size: entry.size,
            version: entry.version.to_text(),
        }
    }
}

#[pymethods]
impl KeyEntry {
    fn __repr__(&self) -> String {
        format!("KeyEntry(key={:?}, size={})", self.key, self.size)
    }
}

/// One page of a listing; `next_start_after` continues it.
#[pyclass(frozen, get_all)]
struct ListPage {
    keys: Vec<Py<KeyEntry>>,
    truncated: bool,
    next_start_after: Option<String>,
}

#[pyclass(frozen, get_all)]
struct Status {
    cluster_id: String,
    cluster_name: Option<String>,
    document_version: u64,
    coordinator: String,
    /// Every node asked: node, build, as `djbod --json status` shows them.
    nodes: Py<PyAny>,
    transport: String,
    /// Every device: device, node, state, label, node_label, total_bytes,
    /// free_bytes, as `djbod --json status` shows them.
    devices: Py<PyAny>,
}

#[pyclass(frozen, get_all)]
struct Identity {
    address: String,
    cluster_id: String,
    cluster_name: Option<String>,
    node: Option<String>,
    build: String,
    document_version: u64,
}

/// A connection to the cluster through any of several nodes.
#[pyclass]
struct Client {
    inner: Mutex<Inner>,
}

/// Raise a class from `djbod.errors` with the given arguments.
fn raise<'py>(py: Python<'py>, class: &str, args: impl pyo3::call::PyCallArgs<'py>) -> PyErr {
    let result = py
        .import("djbod.errors")
        .and_then(|module| module.getattr(class))
        .and_then(|class| class.call1(args));
    match result {
        Ok(instance) => PyErr::from_value(instance),
        Err(e) => e,
    }
}

/// The client's error as the Python exception it corresponds to.
fn to_py(py: Python<'_>, error: ClientError) -> PyErr {
    let message = error.to_string();
    match &error {
        ClientError::Unreachable(attempts) => {
            let attempts: Vec<(String, String)> = attempts
                .iter()
                .map(|(address, reason)| (address.to_string(), reason.clone()))
                .collect();
            raise(py, "Unreachable", (message, attempts))
        }
        _ => match error.detail() {
            Some(detail) => {
                let class = if detail.code == ErrorCode::NotFound {
                    "NotFound"
                } else {
                    "NodeError"
                };
                let detail = match pythonize(py, detail) {
                    Ok(detail) => detail,
                    Err(e) => return e.into(),
                };
                raise(py, class, (message, detail))
            }
            None => raise(py, "ClientError", (message,)),
        },
    }
}

impl Client {
    /// Run `f` on the client with the interpreter lock released.
    fn call<T: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut Inner) -> Result<T, ClientError> + Send,
    ) -> PyResult<T> {
        let result = py.detach(|| {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            f(&mut inner)
        });
        result.map_err(|e| to_py(py, e))
    }
}

#[pymethods]
impl Client {
    /// `nodes` are `host:port` strings tried in order; `cluster` is the
    /// cluster id, learned from the first node that answers when omitted.
    /// `tls_ca`, `tls_cert` and `tls_key` are PEM paths as for `djbod`.
    #[new]
    #[pyo3(signature = (nodes, cluster=None, tls_ca=None, tls_cert=None, tls_key=None))]
    fn new(
        py: Python<'_>,
        nodes: Vec<String>,
        cluster: Option<String>,
        tls_ca: Option<String>,
        tls_cert: Option<String>,
        tls_key: Option<String>,
    ) -> PyResult<Client> {
        let addresses = nodes
            .iter()
            .map(|text| {
                text.parse::<SocketAddr>()
                    .map_err(|e| PyValueError::new_err(format!("node address {text:?}: {e}")))
            })
            .collect::<PyResult<Vec<_>>>()?;
        let cluster = cluster
            .map(|text| {
                Uuid::parse_str(&text)
                    .map_err(|e| PyValueError::new_err(format!("cluster id {text:?}: {e}")))
            })
            .transpose()?;
        let connector = Connector::from_client_options(
            tls_ca.as_deref().map(Path::new),
            tls_cert.as_deref().map(Path::new),
            tls_key.as_deref().map(Path::new),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let mut options = ClientOptions::new(addresses).connector(connector);
        options.cluster = cluster;
        let inner = py
            .detach(|| Inner::connect(options))
            .map_err(|e| to_py(py, e))?;
        Ok(Client {
            inner: Mutex::new(inner),
        })
    }

    #[getter]
    fn cluster_id(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cluster_id()
            .to_string()
    }

    /// The node the client is connected to, or `None` between connections.
    #[getter]
    fn node_address(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .node_address()
            .map(|address| address.to_string())
    }

    /// Store `data` under `key`; returns the version id.
    #[pyo3(signature = (key, data, content_type=None, metadata=None))]
    fn put(
        &self,
        py: Python<'_>,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> PyResult<String> {
        let metadata: BTreeMap<String, String> = metadata.unwrap_or_default().into_iter().collect();
        let size = data.len() as u64;
        let write = self.call(py, |inner| {
            inner.put_from_reader(key, size, data, content_type, metadata)
        })?;
        warn_if_placed_around(py, key, &write)?;
        Ok(write.version.to_text())
    }

    /// Store the file at `path` under `key`, streaming it; returns the
    /// version id.
    #[pyo3(signature = (key, path, content_type=None, metadata=None))]
    fn put_file(
        &self,
        py: Python<'_>,
        key: &str,
        path: &str,
        content_type: Option<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> PyResult<String> {
        let metadata: BTreeMap<String, String> = metadata.unwrap_or_default().into_iter().collect();
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let write = self.call(py, |inner| {
            inner.put_from_reader(key, size, file, content_type, metadata)
        })?;
        warn_if_placed_around(py, key, &write)?;
        Ok(write.version.to_text())
    }

    /// Fetch an object's bytes.
    fn get<'py>(&self, py: Python<'py>, key: &str) -> PyResult<Bound<'py, PyBytes>> {
        let (read, body) = self.call(py, |inner| inner.get(key))?;
        warn_if_degraded(py, key, &read)?;
        Ok(PyBytes::new(py, &body))
    }

    /// Fetch an object into the file at `path`, streaming it. A failure
    /// part way leaves the file with what arrived, and raises.
    fn get_to_file(&self, py: Python<'_>, key: &str, path: &str) -> PyResult<ObjectInfo> {
        let file = File::create(path)?;
        let read = self.call(py, |inner| inner.get_to_writer(key, file))?;
        warn_if_degraded(py, key, &read)?;
        ObjectInfo::from_read(py, read)
    }

    /// The object's record without its body. Warns as `get` does when
    /// the record was trusted without every copy (SPEC 9.4.4).
    fn head(&self, py: Python<'_>, key: &str) -> PyResult<ObjectInfo> {
        let read = self.call(py, |inner| inner.head(key))?;
        warn_if_degraded(py, key, &read)?;
        ObjectInfo::from_read(py, read)
    }

    fn delete(&self, py: Python<'_>, key: &str) -> PyResult<()> {
        self.call(py, |inner| inner.delete(key))
    }

    /// One page of keys, in order.
    #[pyo3(signature = (prefix=None, start_after=None, limit=None))]
    fn list(
        &self,
        py: Python<'_>,
        prefix: Option<String>,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> PyResult<ListPage> {
        let page = self.call(py, |inner| {
            inner.list(ListQuery {
                prefix,
                start_after,
                limit,
            })
        })?;
        Ok(ListPage {
            next_start_after: page.next_start_after().map(str::to_string),
            truncated: page.truncated,
            keys: page
                .keys
                .into_iter()
                .map(|entry| Py::new(py, KeyEntry::from_proto(entry)))
                .collect::<PyResult<_>>()?,
        })
    }

    /// Every key under `prefix`, page after page.
    #[pyo3(signature = (prefix=None))]
    fn list_all(&self, py: Python<'_>, prefix: Option<String>) -> PyResult<Vec<KeyEntry>> {
        let keys = self.call(py, |inner| inner.list_all(prefix.as_deref()))?;
        Ok(keys.into_iter().map(KeyEntry::from_proto).collect())
    }

    /// What one device holds, by UUID: versions, keys, blocks and shard
    /// bytes, as a dict; zero versions means it is empty.
    fn device_contents<'py>(&self, py: Python<'py>, device: &str) -> PyResult<Bound<'py, PyAny>> {
        let id = Uuid::parse_str(device)
            .map_err(|e| PyValueError::new_err(format!("device id {device:?}: {e}")))?;
        let contents = self.call(py, |inner| inner.device_contents(DeviceId(id)))?;
        Ok(pythonize(py, &contents)?)
    }

    /// Rebuild what is damaged or missing of one object; the report as
    /// `djbod --json repair` shows it.
    fn repair<'py>(&self, py: Python<'py>, key: &str) -> PyResult<Bound<'py, PyAny>> {
        let report = self.call(py, |inner| inner.repair(key))?;
        Ok(pythonize(py, &report)?)
    }

    fn status(&self, py: Python<'_>) -> PyResult<Status> {
        let status = self.call(py, |inner| inner.status())?;
        Ok(Status {
            cluster_id: status.cluster_id.to_string(),
            cluster_name: status.cluster_name,
            document_version: status.document_version,
            coordinator: status.coordinator.0.to_string(),
            nodes: pythonize(py, &status.nodes)?.unbind(),
            transport: status.transport.to_string(),
            devices: pythonize(py, &status.devices)?.unbind(),
        })
    }

    /// Who the client is connected to.
    fn identity(&self, py: Python<'_>) -> PyResult<Identity> {
        let identity = self.call(py, |inner| inner.identity())?;
        Ok(Identity {
            address: identity.address.to_string(),
            cluster_id: identity.cluster_id.to_string(),
            cluster_name: identity.cluster_name,
            node: identity.node.map(|node| node.0.to_string()),
            build: identity.build,
            document_version: identity.document_version,
        })
    }

    /// The cluster document as the connected node holds it.
    fn cluster_document<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let document = self.call(py, |inner| inner.cluster_document())?;
        Ok(pythonize(py, &document)?)
    }

    fn __repr__(&self) -> String {
        format!(
            "Client(cluster_id={:?}, node_address={:?})",
            self.cluster_id(),
            self.node_address().unwrap_or_default()
        )
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("BUILD", djbod_client::BUILD)?;
    m.add_class::<Client>()?;
    m.add_class::<ObjectInfo>()?;
    m.add_class::<KeyEntry>()?;
    m.add_class::<ListPage>()?;
    m.add_class::<Status>()?;
    m.add_class::<Identity>()?;
    Ok(())
}
