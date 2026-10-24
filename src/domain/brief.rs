//! The brief: what a searcher agent found for one request, put in front of
//! the request before the agent's turn (`[agents.<a>.brief]`,
//! `bootstrap/brief.rs`). Pure rules only: when an answer counts as a brief,
//! and how the brief and the request are joined.

/// What a searcher answers when nothing in the knowledge base matters.
pub(crate) const NOTHING: &str = "NONE";

/// The searcher's `answer` as a brief, trimmed; `None` when it found nothing:
/// an empty answer, or [`NOTHING`] in any letter case, with or without a
/// final full stop.
pub(crate) fn from_answer(answer: &str) -> Option<String> {
    let answer = answer.trim();
    let bare = answer.strip_suffix('.').unwrap_or(answer).trim_end();
    if answer.is_empty() || bare.eq_ignore_ascii_case(NOTHING) {
        return None;
    }
    Some(answer.to_string())
}

/// `request` with `brief` in front, in a marked block naming the `searcher`,
/// so the agent can tell what it was told from what it was asked.
pub(crate) fn prepend(searcher: &str, brief: &str, request: &str) -> String {
    format!(
        "[Brief from agent `{searcher}`: what the user's knowledge base says that matters for this request]\n\
         {brief}\n\
         [End of brief]\n\n\
         {request}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_found_is_no_brief() {
        for answer in ["", "  \n", "NONE", "none", " None. ", "NONE\n"] {
            assert_eq!(from_answer(answer), None, "{answer:?}");
        }
    }

    #[test]
    fn anything_else_is_the_brief_trimmed() {
        assert_eq!(
            from_answer("\n- Allergic to penicillin (Health.md:5-5)\n"),
            Some("- Allergic to penicillin (Health.md:5-5)".to_string())
        );
        assert_eq!(
            from_answer("None of the notes say so, but: vegetarian."),
            Some("None of the notes say so, but: vegetarian.".to_string()),
            "only a bare NONE means nothing"
        );
    }

    #[test]
    fn the_brief_leads_the_request_in_a_marked_block() {
        let out = prepend(
            "searcher",
            "- Allergic to penicillin",
            "Prescribe an antibiotic.",
        );
        assert_eq!(
            out,
            "[Brief from agent `searcher`: what the user's knowledge base says that matters for this request]\n\
             - Allergic to penicillin\n\
             [End of brief]\n\n\
             Prescribe an antibiotic."
        );
    }
}
