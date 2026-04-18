use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}: bad variable name")]
pub(crate) struct InvalidVariableName(pub(crate) String);

pub(crate) fn ensure_valid_variable_name(name: &str) -> Result<(), InvalidVariableName> {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return Err(InvalidVariableName(name.to_owned())),
    }
    if chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(InvalidVariableName(name.to_owned()))
    }
}
