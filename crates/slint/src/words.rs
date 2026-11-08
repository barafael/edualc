//! Text that isn't a closed set of ours: words people read, or someone else's (a transcript
//! Claude Code wrote). A closed set we own is an enum instead (`lint/rules/static-str-wants-enum.yml`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display, derive_more::Deref)]
pub(crate) struct Words(pub(crate) &'static str);

impl Words {
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}
