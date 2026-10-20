//! Public Quick Share state and metadata; transport-independent protocol lives
//! in crate::session. Original rquickshare GPLv3 attribution is retained.
use serde::{Deserialize, Serialize};
pub(crate) mod info;
mod mdns;
pub use mdns::*;
mod mdns_discovery;
pub use mdns_discovery::*;
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum State {
    #[default]
    Initial,
    ReceivedConnectionRequest,
    SentUkeyServerInit,
    SentUkeyClientInit,
    SentUkeyClientFinish,
    SentPairedKeyEncryption,
    ReceivedUkeyClientFinish,
    SentConnectionResponse,
    SentPairedKeyResult,
    SentIntroduction,
    ReceivedPairedKeyResult,
    WaitingForUserConsent,
    NegotiatingTransport,
    ReceivingFiles,
    SendingFiles,
    Disconnected,
    Rejected,
    Cancelled,
    Finished,
}
#[derive(Debug)]
pub enum OutboundPayload {
    Files(Vec<crate::OutboundFile>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TextPayloadType {
    Url,
    Text,
    Wifi,
}
