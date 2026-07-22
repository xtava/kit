use crate::ContentId;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Lease Expired, data is no longer accessible")]
    LeaseExpired,

    #[error("Content with id {0} not found")]
    ContentNotFound(ContentId),

    #[error("Io error in BlobLease: {0}")]
    Io(#[from] std::io::Error),

    #[error("Storage has already been initialized")]
    AlreadyInitializedStorage,

    #[error("Storage has not been initialized")]
    StorageNotInit,

    #[error("Storage location {0} may be corrupt: {1}")]
    StorageDirIoError(PathBuf, std::io::Error),

    #[error("Blob read admission rejected: {0}")]
    ReadAdmission(#[from] wezterm_runtime_admission::AdmissionError),

    #[error("Blob length {declared} exceeds maximum {maximum}")]
    BlobTooLarge { declared: u64, maximum: usize },

    #[error("Blob length {0} cannot be represented on this platform")]
    BlobLengthOverflow(u64),

    #[error("Blob length changed while reading: declared {declared}, observed {observed}")]
    BlobLengthChanged { declared: usize, observed: u64 },

    #[error("Blob content changed while reading: expected {expected}, observed {observed}")]
    BlobContentChanged {
        expected: ContentId,
        observed: ContentId,
    },
}
