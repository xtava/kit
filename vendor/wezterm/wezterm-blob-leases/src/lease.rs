use crate::{get_storage, BlobReaderSource, ContentId, Error, LeaseId};
use sha2::Digest;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use wezterm_runtime_admission::{
    ByteClass, BytePermit, CountClass, CountPermit, RuntimeAdmission, MAX_BLOB_READER_BUFFER_BYTES,
    MAX_BLOB_READ_WORKING_BYTES_TOTAL,
};

/// A lease represents a handle to data in the store.
/// The lease will help to keep the data alive in the store.
/// Depending on the policy configured for the store, it
/// may guarantee to keep the data intact for its lifetime,
/// or in some cases, it the store is being thrashed and at
/// capacity, it may have been evicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobLease {
    inner: Arc<LeaseInner>,
}

#[derive(Debug, PartialEq, Eq)]
struct LeaseInner {
    pub content_id: ContentId,
    pub lease_id: LeaseId,
}

impl BlobLease {
    pub(crate) fn make_lease(content_id: ContentId, lease_id: LeaseId) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                content_id,
                lease_id,
            }),
        }
    }

    /// Opens an admitted, integrity-checked reader and retains its exact byte charge until drop.
    pub fn reader(&self) -> Result<AdmittedBlobReader, Error> {
        self.reader_with_limit(MAX_BLOB_READ_WORKING_BYTES_TOTAL)
    }

    /// Opens an admitted reader only when its declared size is within `maximum`.
    pub fn reader_with_limit(&self, maximum: usize) -> Result<AdmittedBlobReader, Error> {
        let registered = get_storage()?;
        let reader_permit = registered
            .read_admission
            .try_count(CountClass::BlobReader, 1)?;
        let source = registered.storage.open_reader(self.inner.content_id)?;
        AdmittedBlobReader::from_source(
            source,
            maximum,
            Some(self.inner.content_id),
            Some(self.clone()),
            reader_permit,
            &registered.read_admission,
        )
    }

    /// Materializes admitted bytes and retains both permits with the returned owner.
    pub fn read(&self) -> Result<AdmittedBlob, Error> {
        self.reader()?.read_all()
    }

    /// Materializes admitted bytes only when the declared size is within `maximum`.
    pub fn read_with_limit(&self, maximum: usize) -> Result<AdmittedBlob, Error> {
        self.reader_with_limit(maximum)?.read_all()
    }

    /// Materializes a blob only when its declared and observed sizes exactly match `expected`.
    pub fn read_exact(&self, expected: usize) -> Result<AdmittedBlob, Error> {
        let reader = self.reader_with_limit(expected)?;
        if reader.len() != expected {
            return Err(Error::BlobLengthChanged {
                declared: expected,
                observed: reader.len() as u64,
            });
        }
        reader.read_all()
    }

    pub fn content_id(&self) -> ContentId {
        self.inner.content_id
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        if let Ok(storage) = get_storage() {
            storage
                .storage
                .advise_lease_dropped(self.lease_id, self.content_id)
                .ok();
        }
    }
}

/// Seekable blob reader that owns its reader-count and exact byte permits.
pub struct AdmittedBlobReader {
    reader: Box<dyn crate::BufSeekRead + Send + Sync>,
    declared_len: usize,
    expected_content_id: Option<ContentId>,
    _reader_permit: CountPermit,
    byte_permit: BytePermit,
    lease: Option<BlobLease>,
}

impl std::fmt::Debug for AdmittedBlobReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedBlobReader")
            .field("declared_len", &self.declared_len)
            .field("expected_content_id", &self.expected_content_id)
            .finish_non_exhaustive()
    }
}

impl AdmittedBlobReader {
    fn from_source(
        mut source: BlobReaderSource,
        maximum: usize,
        expected_content_id: Option<ContentId>,
        lease: Option<BlobLease>,
        reader_permit: CountPermit,
        admission: &RuntimeAdmission,
    ) -> Result<Self, Error> {
        if source.declared_len > maximum as u64 {
            return Err(Error::BlobTooLarge {
                declared: source.declared_len,
                maximum,
            });
        }
        let declared_len = usize::try_from(source.declared_len)
            .map_err(|_| Error::BlobLengthOverflow(source.declared_len))?;
        let byte_permit = admission.try_bytes(ByteClass::BlobReadWorking, declared_len)?;
        verify_reader(&mut source.reader, declared_len, expected_content_id)?;

        Ok(Self {
            reader: source.reader,
            declared_len,
            expected_content_id,
            _reader_permit: reader_permit,
            byte_permit,
            lease,
        })
    }

    pub fn len(&self) -> usize {
        self.declared_len
    }

    pub fn is_empty(&self) -> bool {
        self.declared_len == 0
    }

    /// Reads exactly the admitted length, rechecks integrity, and transfers both permits to the
    /// returned byte owner.
    pub fn read_all(self) -> Result<AdmittedBlob, Error> {
        let Self {
            mut reader,
            declared_len,
            expected_content_id,
            _reader_permit: reader_permit,
            byte_permit,
            lease,
        } = self;
        let data = materialize_exact(&mut reader, declared_len, expected_content_id)?;

        Ok(AdmittedBlob {
            data,
            reader_permit,
            byte_permit,
            lease,
        })
    }
}

impl Read for AdmittedBlobReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl BufRead for AdmittedBlobReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.reader.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.reader.consume(amount)
    }
}

impl Seek for AdmittedBlobReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.reader.seek(position)
    }
}

/// Materialized blob bytes that keep read admission until their final use completes.
#[derive(Debug)]
pub struct AdmittedBlob {
    data: Vec<u8>,
    reader_permit: CountPermit,
    byte_permit: BytePermit,
    lease: Option<BlobLease>,
}

impl AdmittedBlob {
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Moves the bytes while preserving a guard that callers must retain through their use.
    pub fn into_guarded_parts(self) -> (Vec<u8>, BlobReadGuard) {
        let Self {
            data,
            reader_permit,
            byte_permit,
            lease,
        } = self;
        (
            data,
            BlobReadGuard {
                _reader_permit: reader_permit,
                _byte_permit: byte_permit,
                _lease: lease,
            },
        )
    }
}

/// Admission guard for APIs that require ownership of the underlying `Vec<u8>`.
#[derive(Debug)]
pub struct BlobReadGuard {
    _reader_permit: CountPermit,
    _byte_permit: BytePermit,
    _lease: Option<BlobLease>,
}

/// Reads an external file through the same process-wide blob admission owner.
pub fn read_file_with_limit(path: impl AsRef<Path>, maximum: usize) -> Result<AdmittedBlob, Error> {
    let registered = get_storage()?;
    let reader_permit = registered
        .read_admission
        .try_count(CountClass::BlobReader, 1)?;
    let file = std::fs::File::open(path)?;
    let declared_len = file.metadata()?.len();
    let source = BlobReaderSource::new(std::io::BufReader::new(file), declared_len);
    AdmittedBlobReader::from_source(
        source,
        maximum,
        None,
        None,
        reader_permit,
        &registered.read_admission,
    )?
    .read_all()
}

/// Copies already-retained bytes through the same reader-count and exact-byte admission owner.
pub fn copy_bytes_with_limit(data: &[u8], maximum: usize) -> Result<AdmittedBlob, Error> {
    if data.len() > maximum {
        return Err(Error::BlobTooLarge {
            declared: data.len() as u64,
            maximum,
        });
    }
    let registered = get_storage()?;
    let reader_permit = registered
        .read_admission
        .try_count(CountClass::BlobReader, 1)?;
    let byte_permit = registered
        .read_admission
        .try_bytes(ByteClass::BlobReadWorking, data.len())?;

    Ok(AdmittedBlob {
        data: data.to_vec(),
        reader_permit,
        byte_permit,
        lease: None,
    })
}

fn verify_reader(
    reader: &mut (dyn crate::BufSeekRead + Send + Sync),
    declared_len: usize,
    expected_content_id: Option<ContentId>,
) -> Result<(), Error> {
    reader.rewind()?;
    let mut hasher = expected_content_id.map(|_| sha2::Sha256::new());
    let mut observed = 0usize;
    let mut buffer = vec![0; MAX_BLOB_READER_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read)
            .ok_or(Error::BlobLengthOverflow(u64::MAX))?;
        if observed > declared_len {
            let actual = reader.seek(SeekFrom::End(0))?;
            return Err(Error::BlobLengthChanged {
                declared: declared_len,
                observed: actual,
            });
        }
        if let Some(hasher) = &mut hasher {
            hasher.update(&buffer[..read]);
        }
    }
    if observed != declared_len {
        return Err(Error::BlobLengthChanged {
            declared: declared_len,
            observed: observed as u64,
        });
    }
    if let (Some(expected), Some(hasher)) = (expected_content_id, hasher) {
        verify_content_id(expected, hasher)?;
    }
    reader.rewind()?;
    Ok(())
}

fn materialize_exact(
    reader: &mut (dyn crate::BufSeekRead + Send + Sync),
    declared_len: usize,
    expected_content_id: Option<ContentId>,
) -> Result<Vec<u8>, Error> {
    reader.rewind()?;
    let mut data = vec![0; declared_len];
    let mut observed = 0usize;
    while observed < declared_len {
        let read = reader.read(&mut data[observed..])?;
        if read == 0 {
            return Err(Error::BlobLengthChanged {
                declared: declared_len,
                observed: observed as u64,
            });
        }
        observed += read;
    }
    let mut extra = [0u8; 1];
    if reader.read(&mut extra)? != 0 {
        let actual = reader.seek(SeekFrom::End(0))?;
        return Err(Error::BlobLengthChanged {
            declared: declared_len,
            observed: actual,
        });
    }
    let observed_content_id = ContentId::for_bytes(&data);
    if let Some(expected) = expected_content_id {
        if observed_content_id != expected {
            return Err(Error::BlobContentChanged {
                expected,
                observed: observed_content_id,
            });
        }
    }
    Ok(data)
}

fn verify_content_id(expected: ContentId, hasher: sha2::Sha256) -> Result<(), Error> {
    let observed = ContentId::from_hash_bytes(hasher.finalize().into());
    if observed != expected {
        return Err(Error::BlobContentChanged { expected, observed });
    }
    Ok(())
}

/// Serialize a lease to/from its content id.
/// This can fail in either direction if the lease is stale
/// during serialization, or if the data for that content id
/// is not available during deserialization.
#[cfg(feature = "serde")]
pub mod lease_content_id {

    use super::*;
    use crate::BlobManager;
    use serde::{de, ser, Deserialize, Serialize};

    /// Serialize a lease as its content id
    pub fn serialize<S>(lease: &BlobLease, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        lease.inner.content_id.serialize(serializer)
    }

    /// Deserialize a lease from a content id.
    /// Will fail unless the content id is already available
    pub fn deserialize<'de, D>(d: D) -> Result<BlobLease, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let content_id = <ContentId as Deserialize>::deserialize(d)?;
        BlobManager::get_by_content_id(content_id)
            .map_err(|err| de::Error::custom(format!("{err:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlobStorage, LeaseId};
    use std::io::Cursor;
    use std::sync::Mutex;
    use wezterm_runtime_admission::{
        RuntimeRole, MAX_BLOB_READERS_TOTAL, MAX_BLOB_READ_WORKING_BYTES_TOTAL,
    };

    static REGISTERED_STORAGE_TEST: Mutex<()> = Mutex::new(());

    fn test_admission() -> Arc<RuntimeAdmission> {
        RuntimeAdmission::new(RuntimeRole::Client).unwrap()
    }

    fn admitted_test_reader(
        data: Vec<u8>,
        declared_len: u64,
        expected_content_id: Option<ContentId>,
        maximum: usize,
        admission: &Arc<RuntimeAdmission>,
    ) -> Result<AdmittedBlobReader, Error> {
        let reader_permit = admission.try_count(CountClass::BlobReader, 1)?;
        AdmittedBlobReader::from_source(
            BlobReaderSource::new(Cursor::new(data), declared_len),
            maximum,
            expected_content_id,
            None,
            reader_permit,
            admission,
        )
    }

    #[test]
    fn exact_boundary_is_admitted_and_one_byte_over_is_rejected() {
        let admission = test_admission();
        let reader = admitted_test_reader(vec![1, 2, 3], 3, None, 3, &admission).unwrap();
        assert_eq!(reader.read_all().unwrap().as_slice(), &[1, 2, 3]);

        assert!(matches!(
            admitted_test_reader(vec![1, 2, 3, 4], 4, None, 3, &admission),
            Err(Error::BlobTooLarge {
                declared: 4,
                maximum: 3
            })
        ));
    }

    #[test]
    fn short_grown_and_same_length_changed_sources_are_rejected() {
        let admission = test_admission();
        assert!(matches!(
            admitted_test_reader(vec![1, 2], 3, None, 3, &admission),
            Err(Error::BlobLengthChanged {
                declared: 3,
                observed: 2
            })
        ));
        assert!(matches!(
            admitted_test_reader(vec![1, 2, 3, 4], 3, None, 3, &admission),
            Err(Error::BlobLengthChanged {
                declared: 3,
                observed: 4
            })
        ));

        let expected = ContentId::for_bytes(&[1, 2, 3]);
        assert!(matches!(
            admitted_test_reader(vec![3, 2, 1], 3, Some(expected), 3, &admission),
            Err(Error::BlobContentChanged { expected: found, .. }) if found == expected
        ));
    }

    #[test]
    fn dropping_an_admitted_reader_releases_count_and_exact_byte_permits() {
        let admission = test_admission();
        let reader = admitted_test_reader(vec![1, 2, 3], 3, None, 3, &admission).unwrap();
        let count_fill = admission
            .try_count(CountClass::BlobReader, MAX_BLOB_READERS_TOTAL - 1)
            .unwrap();
        let byte_fill = admission
            .try_bytes(
                ByteClass::BlobReadWorking,
                MAX_BLOB_READ_WORKING_BYTES_TOTAL - 3,
            )
            .unwrap();
        assert!(admission.try_count(CountClass::BlobReader, 1).is_err());
        assert!(admission.try_bytes(ByteClass::BlobReadWorking, 1).is_err());

        drop(reader);
        let released_count = admission.try_count(CountClass::BlobReader, 1).unwrap();
        let released_byte = admission.try_bytes(ByteClass::BlobReadWorking, 1).unwrap();
        drop((released_count, released_byte, count_fill, byte_fill));
    }

    #[test]
    fn concurrent_reader_saturation_rejects_then_recovers_after_drop() {
        let admission = test_admission();
        let mut readers = (0..MAX_BLOB_READERS_TOTAL)
            .map(|_| admitted_test_reader(Vec::new(), 0, None, 0, &admission).unwrap())
            .collect::<Vec<_>>();

        assert!(matches!(
            admitted_test_reader(Vec::new(), 0, None, 0, &admission),
            Err(Error::ReadAdmission(_))
        ));
        readers.pop();
        assert!(admitted_test_reader(Vec::new(), 0, None, 0, &admission).is_ok());
    }

    struct MemoryStorage {
        data: Vec<u8>,
    }

    impl BlobStorage for MemoryStorage {
        fn store(
            &self,
            _content_id: ContentId,
            _data: &[u8],
            _lease_id: LeaseId,
        ) -> Result<(), Error> {
            Ok(())
        }

        fn lease_by_content(
            &self,
            _content_id: ContentId,
            _lease_id: LeaseId,
        ) -> Result<(), Error> {
            Ok(())
        }

        fn open_reader(&self, _content_id: ContentId) -> Result<BlobReaderSource, Error> {
            Ok(BlobReaderSource::new(
                Cursor::new(self.data.clone()),
                self.data.len() as u64,
            ))
        }

        fn advise_lease_dropped(
            &self,
            _lease_id: LeaseId,
            _content_id: ContentId,
        ) -> Result<(), Error> {
            Ok(())
        }

        fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
            Ok(())
        }

        fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn lease_reads_charge_the_injected_process_admission() {
        let _serial = REGISTERED_STORAGE_TEST.lock().unwrap();
        let data = vec![1, 2, 3];
        let admission = test_admission();
        crate::register_storage(
            Arc::new(MemoryStorage { data: data.clone() }),
            Arc::clone(&admission),
        )
        .unwrap();
        let lease = BlobLease::make_lease(ContentId::for_bytes(&data), LeaseId::new());
        let saturation = admission
            .try_count(CountClass::BlobReader, MAX_BLOB_READERS_TOTAL)
            .unwrap();

        assert!(matches!(lease.reader(), Err(Error::ReadAdmission(_))));
        drop(saturation);
        assert_eq!(lease.read().unwrap().as_slice(), data);

        drop(lease);
        crate::clear_storage();
    }
}
