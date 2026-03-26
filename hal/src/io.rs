pub enum IoError {}

pub type IoResult<T> = Result<T, IoError>;
