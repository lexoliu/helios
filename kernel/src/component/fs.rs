#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentFsNodeKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentResourceTableError {
    NotPresent,
    WrongType,
    HasChildren,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentFsResourceError {
    BadDescriptor,
    Busy,
    Overflow,
}

pub fn map_resource_table_error(error: ComponentResourceTableError) -> ComponentFsResourceError {
    match error {
        ComponentResourceTableError::NotPresent | ComponentResourceTableError::WrongType => {
            ComponentFsResourceError::BadDescriptor
        }
        ComponentResourceTableError::HasChildren => ComponentFsResourceError::Busy,
        ComponentResourceTableError::Full => ComponentFsResourceError::Overflow,
    }
}
