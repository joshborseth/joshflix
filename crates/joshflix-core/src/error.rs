use thiserror::Error;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Every failure the core can report. `Display` strings are safe to show on screen.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("Couldn't reach {service}. Check the TV's internet connection.")]
    Network {
        service: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{service} returned an error ({status}).")]
    Status { service: &'static str, status: u16 },
    #[error("{service} sent a response Joshflix couldn't read.")]
    Decode {
        service: &'static str,
        detail: String,
    },
    #[error("{0}")]
    RealDebrid(#[from] crate::realdebrid::RdError),
    #[error("{0}")]
    Relay(String),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("{0}")]
    Invalid(String),
    #[error("Cancelled")]
    Cancelled,
}

impl CoreError {
    pub(crate) fn decode(service: &'static str, detail: impl ToString) -> Self {
        Self::Decode {
            service,
            detail: detail.to_string(),
        }
    }
}

impl From<redb::Error> for CoreError {
    fn from(error: redb::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for CoreError {
            fn from(error: $t) -> Self {
                Self::Storage(error.to_string())
            }
        }
    )*};
}

storage_from!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    std::io::Error
);
