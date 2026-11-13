//! Code in colour, as a window that draws it is asked and told: a block to colour, and the same
//! block coloured. A window that colours code (the Slint window does, on a thread of its own)
//! makes `Job`s of the conversation's code blocks, and the app hands it back what is coloured as
//! the `Event::CodeColoured` it comes as (see `Frontend::code_coloured`); the colouring itself is
//! the window's.

/// Where a conversation's code block is: its entry, and its place among the entry's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockAt {
    pub entry: usize,
    pub block: usize,
}

/// Which drawing of the conversation a colouring is for: each time the window drops its blocks
/// (another conversation, other colours), what was asked before is for one no longer shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drawing(u64);

impl Drawing {
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// A code block to colour.
#[derive(Debug)]
pub struct Job {
    pub at: BlockAt,
    pub drawing: Drawing,
    pub lang: String,
    pub code: String,
}

/// A code block coloured: the code it was asked for, back, so the window can tell whether it is
/// still the block's; its Markdown, or `None` where it stays plain.
#[derive(Debug)]
pub struct Coloured {
    pub at: BlockAt,
    pub drawing: Drawing,
    pub code: String,
    pub markdown: Option<String>,
}
