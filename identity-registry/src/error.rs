use thiserror::Error;

#[derive(Error, Debug)]
pub enum IdentityRegistryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("F1R3FLY node error: {0}")]
    Node(String),

    #[error("secp256k1 error: {0}")]
    Secp256k1(#[from] secp256k1::Error),

    #[error("contract rejected the request: {0}")]
    Rejected(String),

    #[error("could not parse contract response: {0}")]
    ResponseParse(String),

    #[error("timed out waiting for contract response")]
    ResponseTimeout,
}
