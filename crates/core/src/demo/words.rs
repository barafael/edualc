//! Text that isn't a closed set of ours: words people read, or someone else's (a transcript
//! Claude Code wrote). A closed set we own is an enum instead (`lint/rules/static-str-wants-enum.yml`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display, derive_more::Deref)]
pub struct Words(pub &'static str);

impl Words {
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}
