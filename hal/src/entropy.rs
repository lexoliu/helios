#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyQuality {
    Cryptographic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntropyUnavailable;
