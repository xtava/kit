use crate::{ContentId, Error, LeaseId};
use std::io::{BufRead, Seek};
use std::sync::{Arc, Mutex};
use wezterm_runtime_admission::RuntimeAdmission;

static STORAGE: Mutex<Option<Arc<RegisteredStorage>>> = Mutex::new(None);

pub trait BufSeekRead: BufRead + Seek {}
impl<T: BufRead + Seek> BufSeekRead for T {}
type BoxedReader = Box<dyn BufSeekRead + Send + Sync>;

/// Storage-owned reader plus the exact byte length observed when it was opened.
///
/// The fields stay private so callers cannot bypass `BlobLease` admission. External storage
/// implementations construct this value and the lease turns it into an admitted reader.
pub struct BlobReaderSource {
    pub(crate) reader: BoxedReader,
    pub(crate) declared_len: u64,
}

impl BlobReaderSource {
    pub fn new<R>(reader: R, declared_len: u64) -> Self
    where
        R: BufRead + Seek + Send + Sync + 'static,
    {
        Self {
            reader: Box::new(reader),
            declared_len,
        }
    }
}

pub(crate) struct RegisteredStorage {
    pub(crate) storage: Arc<dyn BlobStorage + Send + Sync + 'static>,
    pub(crate) read_admission: Arc<RuntimeAdmission>,
}

/// Implements the actual storage mechanism for blobs
pub trait BlobStorage {
    /// Store data with the provided content_id.
    /// lease_id is provided by the caller to identify this store.
    /// The underlying store is expected to dedup storing data with the same
    /// content_id.
    fn store(&self, content_id: ContentId, data: &[u8], lease_id: LeaseId) -> Result<(), Error>;

    /// Resolve the data associated with content_id.
    /// If found, establish a lease with the given lease_id.
    /// If not found, returns Err(Error::ContentNotFound)
    fn lease_by_content(&self, content_id: ContentId, lease_id: LeaseId) -> Result<(), Error>;

    /// Opens storage-owned bytes and reports their exact length at open time. `BlobLease` owns
    /// admission, integrity verification and lease lifetime around this raw storage operation.
    fn open_reader(&self, content_id: ContentId) -> Result<BlobReaderSource, Error>;

    /// Advises the storage manager that a particular lease has been dropped.
    fn advise_lease_dropped(&self, lease_id: LeaseId, content_id: ContentId) -> Result<(), Error>;
    /// Advises the storage manager that a given process id is now, or
    /// continues to be, alive and a valid consumer of the store.
    fn advise_of_pid(&self, pid: u32) -> Result<(), Error>;

    /// Advises the storage manager that a given process id is, or will
    /// very shortly, terminate and will cease to be a valid consumer
    /// of the store.
    /// It may choose to do something to invalidate all leases with
    /// a corresponding pid.
    fn advise_pid_terminated(&self, pid: u32) -> Result<(), Error>;
}

pub fn register_storage(
    storage: Arc<dyn BlobStorage + Send + Sync + 'static>,
    read_admission: Arc<RuntimeAdmission>,
) -> Result<(), Error> {
    STORAGE.lock().unwrap().replace(Arc::new(RegisteredStorage {
        storage,
        read_admission,
    }));
    Ok(())
}

pub(crate) fn get_storage() -> Result<Arc<RegisteredStorage>, Error> {
    STORAGE
        .lock()
        .unwrap()
        .as_ref()
        .cloned()
        .ok_or_else(|| Error::StorageNotInit)
}

pub fn clear_storage() {
    STORAGE.lock().unwrap().take();
}
