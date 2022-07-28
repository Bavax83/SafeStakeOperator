
pub fn require(status: bool, msg: &'static str) {
    if !status {
        panic!("{}", msg);
    }
}


#[derive(Clone, Debug, PartialEq)]
pub enum DvfError {
    /// The consensus protocol failed.
    ConsensusFailure(String),
    /// Key generation failed.
    KeyGenError(String),
    /// Threshold signature aggregation failed due to insufficient valid signatures.
    InsufficientSignatures {got: usize, expected: usize},
    /// Threshold signature aggregation failed due to insufficient valid signatures.
    InsufficientValidSignatures {got: usize, expected: usize},
    /// Invalid operator signature
    InvalidSignatureShare {id: u64},
    /// Invalid operator id 
    InvalidOperatorId {id: u64},
    /// Different length
    DifferentLength {x: usize, y: usize},
    /// 
    InvalidLength,
    /// Should not call the function specified by the string
    UnexpectedCall(String),
    ///
    Unknown
}
