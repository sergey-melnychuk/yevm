use thiserror::Error;

pub use yaevmi_base::{Acc, Int};

pub use crate::call::{Call, Head, Tx};
pub use crate::evm::Fetch;

pub mod aux;
pub mod cache;
pub mod call;
pub mod chain;
pub mod eip7702;
pub mod evm;
pub mod exe;
pub mod ops;
pub mod pre;
pub mod rpc;
pub mod state;
pub mod trace;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Undefined chain id")]
    UndefinedChainId,
    #[error("Gas too low: have {have} but want {want}")]
    GasTooLow { have: u64, want: u64 },
    #[error("Insufficient account funds")]
    InsufficientFunds,
    #[error("Sender is not an EOA")]
    SenderNotEOA,
    #[error("Max fee per gas less than base fee")]
    MaxFeeLessThanBaseFee,
    #[error("Priority fee greater than max fee")]
    PriorityGreaterThanMaxFee,
    #[error("Gas limit exceeds block gas limit")]
    GasAllowanceExceeded,
    #[error("Gas limit times gas price overflow")]
    GasLimitPriceProductOverflow,
    #[error("Error: {0}")]
    Generic(#[from] eyre::Report),
}

pub type Result<T> = std::result::Result<T, Error>;
