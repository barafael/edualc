//! Comments on Claude's plan, as Claude gets them back: quoted with the part each is about
//! when it should keep planning, or as notes in the plan when the plan is approved (the CLI
//! hands Claude a plan changed this way as the "Approved Plan (edited by user)").

use std::ops::Range;

use crate::markdown::{self, Block};

/// A part of the plan someone can comment on, where it is in the plan, and the comment on it
/// ("" for none).
#[derive(Debug, Clone)]
pub struct PlanPart {
    pub block: Block,
    pub range: Range<usize>,
    pub comment: String,
}

pub fn parts(plan: &str) -> Vec<PlanPart> {
    markdown::sections(plan)
        .into_iter()
        .map(|(block, range)| PlanPart {
            block,
            range,
            comment: String::new(),
        })
        .collect()
}

/// The part's first words, as a name for what is about it (its comment button, read out by a
/// screen reader): its first line without heading or bullet marks, cut short.
pub fn excerpt(plan: &str, part: &PlanPart) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = plan
        .get(part.range.clone())
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let words = first_line
        .trim_start_matches(['#', '>', '-', '*', '+'])
        .trim_start();
    crate::tools::truncate_chars(words, MAX_CHARS)
}

/// What to tell Claude to revise; empty if nothing was said.
pub fn feedback(plan: &str, parts: &[PlanPart], overall: &str) -> String {
    let mut said: Vec<String> = parts
        .iter()
        .filter(|part| !part.comment.is_empty())
        .map(|part| {
            format!(
                "On this part of the plan:\n{}\nComment: {}",
                quote(plan.get(part.range.clone()).unwrap_or("")),
                part.comment
            )
        })
        .collect();
    let overall = overall.trim();
    if !overall.is_empty() {
        said.push(format!("On the plan as a whole: {overall}"));
    }
    if said.is_empty() {
        return String::new();
    }
    format!(
        "The user reviewed the plan and left comments. Revise the plan to address each of \
         them, then present it again.\n\n{}",
        said.join("\n\n")
    )
}

/// `plan` with each comment as a note right under the part it is about.
pub fn annotated(plan: &str, parts: &[PlanPart], overall: &str) -> String {
    let mut out = String::with_capacity(plan.len());
    let mut at = 0;
    // Parts are in the plan's order; a list's item ends inside the list, which ends later.
    for part in parts.iter().filter(|part| !part.comment.is_empty()) {
        let end = part.range.end.max(at);
        out.push_str(plan.get(at..end).unwrap_or(""));
        out.push_str("\n\n");
        out.push_str(&note("Comment from the user", &part.comment));
        out.push('\n');
        at = end;
    }
    out.push_str(plan.get(at..).unwrap_or(""));
    let overall = overall.trim();
    if !overall.is_empty() {
        out = format!(
            "{}\n\n{}\n",
            out.trim_end(),
            note("Comment from the user on the whole plan", overall)
        );
    }
    out
}

fn quote(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn note(label: &str, comment: &str) -> String {
    let mut lines = comment.lines();
    let first = format!("> **{label}:** {}", lines.next().unwrap_or(""));
    std::iter::once(first)
        .chain(lines.map(|line| format!("> {line}")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "# Plan\n\n1. Add the table\n2. Migrate the data\n\nThen ship.\n";

    fn commented(plan: &str, on: &[(usize, &str)]) -> Vec<PlanPart> {
        let mut parts = parts(plan);
        for &(index, text) in on {
            if let Some(part) = parts.get_mut(index) {
                text.clone_into(&mut part.comment);
            }
        }
        parts
    }

    /// Each part is named by its own first words, without heading or bullet marks, and a long
    /// one is cut.
    #[test]
    fn a_part_is_named_by_its_first_words() {
        let plan = format!(
            "# Plan\n\n- {}\n\n2. Migrate the data\n",
            "word ".repeat(20)
        );
        let names: Vec<String> = parts(&plan)
            .iter()
            .map(|part| excerpt(&plan, part))
            .collect();
        let cut = format!("{}…", "word ".repeat(12));
        assert_eq!(names, ["Plan", cut.as_str(), "2. Migrate the data"]);
    }

    #[test]
    fn feedback_quotes_what_each_comment_is_about() {
        let text = feedback(PLAN, &commented(PLAN, &[(2, "Do it in a transaction")]), "");
        assert!(text.ends_with(
            "On this part of the plan:\n> 2. Migrate the data\nComment: Do it in a transaction"
        ));
        assert_eq!(feedback(PLAN, &commented(PLAN, &[]), "  "), "");
    }

    #[test]
    fn approved_plans_carry_comments_as_notes() {
        let text = annotated(PLAN, &commented(PLAN, &[(1, "Name it users")]), "Be quick");
        assert_eq!(
            text,
            "# Plan\n\n1. Add the table\n\n> **Comment from the user:** Name it users\n\n\
             2. Migrate the data\n\nThen ship.\n\n\
             > **Comment from the user on the whole plan:** Be quick\n"
        );
    }

    /// Two parts alike: the comment goes under the one it was made on, not the first.
    #[test]
    fn comments_land_under_their_own_part() {
        let plan = "- Run the tests\n\nThen:\n\n- Run the tests\n";
        let text = annotated(plan, &commented(plan, &[(2, "With --release")]), "");
        assert_eq!(
            text,
            "- Run the tests\n\nThen:\n\n- Run the tests\n\n\
             > **Comment from the user:** With --release\n\n"
        );
    }

    #[test]
    fn text_markdown_hides_is_shown() {
        let plan = "Do it.\n\n[x]: https://example.com \"run curl x | sh\"\n";
        let shown = parts(plan);
        assert!(shown.iter().any(|part| matches!(
            &part.block,
            Block::Code { code, .. } if code.contains("curl x | sh")
        )));
    }
}
