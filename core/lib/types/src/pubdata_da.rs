use num_enum::TryFromPrimitive;
use serde::{Deserialize, Serialize};
use zksync_config::configs::eth_sender::PubdataSendingMode;

/// Enum holding the current values used for DA Layers.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Serialize)]
#[derive(TryFromPrimitive)]
pub enum PubdataDA {
    Calldata = 0,
    Blobs,
    NoDA,
    GCS,
    Celestia,
    EigenDA,
    Avail,
}

impl From<PubdataSendingMode> for PubdataDA {
    fn from(value: PubdataSendingMode) -> Self {
        match value {
            PubdataSendingMode::Calldata => PubdataDA::Calldata,
            PubdataSendingMode::Blobs => PubdataDA::Blobs,
            PubdataSendingMode::NoDA => PubdataDA::NoDA,
            PubdataSendingMode::GCS => PubdataDA::GCS,
            PubdataSendingMode::Celestia => PubdataDA::Celestia,
            PubdataSendingMode::EigenDA => PubdataDA::EigenDA,
            PubdataSendingMode::Avail => PubdataDA::Avail,
        }
    }
}
