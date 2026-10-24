//! SKILL.md frontmatter — the one split every reader uses (the registry,
//! the planner registry, `view_skill`, `manage_skill`, the lifecycle tools,
//! `tengu skill` / `eval` / `evolve`, `run-agent`), so a skill is either
//! visible to all of them or to none.
//!
//! | Rule | Value |
//! |---|---|
//! | Before the opening fence | an optional BOM, then whitespace / blank lines |
//! | Fence | a line of three or more `-` and nothing else (trailing spaces and `\r` ignored: CRLF files split) |
//! | YAML | the lines between the fences, the last newline included |
//! | Body | everything after the closing fence line, untrimmed |

use std::fmt;

/// A SKILL.md split at its YAML frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frontmatter<'a> {
    /// The text between the fences, its last newline included (empty when
    /// the fences touch).
    pub(crate) yaml: &'a str,
    /// Everything after the closing fence line.
    pub(crate) body: &'a str,
}

/// Why a text has no frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontmatterError {
    /// The first non-blank line is not a fence.
    NoOpening,
    /// No fence line after the opening one.
    NotClosed,
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoOpening => "no frontmatter (the first line must be `---`)",
            Self::NotClosed => "frontmatter not closed (no `---` line after the opening one)",
        })
    }
}

impl std::error::Error for FrontmatterError {}

fn is_fence(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == b'-')
}

/// `text` split at its frontmatter (module table).
pub(crate) fn split_frontmatter(text: &str) -> Result<Frontmatter<'_>, FrontmatterError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim_start();
    let mut lines = text.split_inclusive('\n');
    let open = lines
        .next()
        .filter(|l| is_fence(l))
        .ok_or(FrontmatterError::NoOpening)?;
    let yaml_start = open.len();
    let mut at = yaml_start;
    for line in lines {
        if is_fence(line) {
            return Ok(Frontmatter {
                yaml: &text[yaml_start..at],
                body: &text[at + line.len()..],
            });
        }
        at += line.len();
    }
    Err(FrontmatterError::NotClosed)
}

/// The body after the frontmatter; the whole text when it has none.
pub(crate) fn skill_body(text: &str) -> &str {
    split_frontmatter(text).map_or(text, |f| f.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(t: &str) -> (&str, &str) {
        let f = split_frontmatter(t).unwrap();
        (f.yaml, f.body)
    }

    #[test]
    fn lf_crlf_and_indented_skills_split_alike() {
        assert_eq!(
            split("---\nname: x\ndescription: d\n---\n\n# Body\n"),
            ("name: x\ndescription: d\n", "\n# Body\n")
        );
        assert_eq!(
            split("---\r\nname: x\r\n---\r\nBody\r\n"),
            ("name: x\r\n", "Body\r\n")
        );
        assert_eq!(
            split("\u{feff}\n\n  ---\nname: x\n----- \nBody"),
            ("name: x\n", "Body")
        );
    }

    #[test]
    fn edges() {
        assert_eq!(split("---\n---\n"), ("", ""));
        assert_eq!(split("---\nname: x\n---"), ("name: x\n", ""));
        // Only a whole dash line is a fence; the first one closes.
        assert_eq!(
            split("---\na: --- b\n---x\n---\nbody\n---\nmore"),
            ("a: --- b\n---x\n", "body\n---\nmore")
        );
        assert_eq!(
            split_frontmatter("# Title\n---\n"),
            Err(FrontmatterError::NoOpening)
        );
        assert_eq!(
            split_frontmatter("--- name: x\n---\n"),
            Err(FrontmatterError::NoOpening)
        );
        assert_eq!(
            split_frontmatter("---\nname: x\n"),
            Err(FrontmatterError::NotClosed)
        );
        assert_eq!(skill_body("# Plain\n"), "# Plain\n");
        assert_eq!(skill_body("---\nname: x\n---\nBody\n"), "Body\n");
    }
}
