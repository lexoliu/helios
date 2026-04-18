pub(crate) const DEFAULT_IFS: &str = " \t\n";

#[derive(Clone, Copy)]
pub(crate) struct IfsConfig<'a> {
    raw: &'a str,
}

impl<'a> IfsConfig<'a> {
    pub(crate) fn new(raw: &'a str) -> Self {
        Self { raw }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.raw.is_empty()
    }

    pub(crate) fn contains(self, ch: char) -> bool {
        self.raw.contains(ch)
    }

    pub(crate) fn is_whitespace(self, ch: char) -> bool {
        matches!(ch, ' ' | '\t' | '\n') && self.raw.contains(ch)
    }

    pub(crate) fn is_non_whitespace(self, ch: char) -> bool {
        self.raw.contains(ch) && !self.is_whitespace(ch)
    }
}
