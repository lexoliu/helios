use wasmtime::component::ResourceTableError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentFsNodeKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentFsResourceError {
    BadDescriptor,
    Busy,
    Overflow,
}

pub fn map_resource_table_error(error: ResourceTableError) -> ComponentFsResourceError {
    match error {
        ResourceTableError::NotPresent | ResourceTableError::WrongType => {
            ComponentFsResourceError::BadDescriptor
        }
        ResourceTableError::HasChildren => ComponentFsResourceError::Busy,
        ResourceTableError::Full => ComponentFsResourceError::Overflow,
    }
}
