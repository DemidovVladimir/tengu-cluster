//! Terminal approval gate — prints baseline→best delta + unified diff,
//! reads a single keystroke from stdin.

use anyhow::Result;
use similar::{ChangeTag, TextDiff};
use std::io::{BufRead, Write};

pub(crate) enum Decision {
    Apply,
    Discard,
    ShowDetails,
    OpenWorktree,
}

pub(crate) struct GateView<'a> {
    pub skill: &'a str,
    pub target_metric: &'a str,
    pub baseline_target: f32,
    pub best_target: f32,
    pub gated_snapshots: &'a [(String, f32, f32)], // (name, baseline, best)
    pub old_body: &'a str,
    pub new_body: &'a str,
    pub rationale: &'a str,
}

pub(crate) fn render(view: &GateView, w: &mut dyn Write) -> Result<()> {
    writeln!(w, "Skill: {}", view.skill)?;
    writeln!(
        w,
        "Target metric: {} (baseline {:.2} → proposed {:.2}, delta {:+.2})",
        view.target_metric,
        view.baseline_target,
        view.best_target,
        view.best_target - view.baseline_target,
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Non-target gated metrics (must stay >= baseline - 0.05):"
    )?;
    for (name, b, p) in view.gated_snapshots {
        let ok = *p + 0.05 >= *b;
        writeln!(
            w,
            "  {:<20}  {:.2} → {:.2}   {}",
            name,
            b,
            p,
            if ok { "✓" } else { "✗" }
        )?;
    }
    writeln!(w)?;
    writeln!(w, "SKILL.md changes (unified diff):")?;
    let diff = TextDiff::from_lines(view.old_body, view.new_body);
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        write!(w, "  {sign}{}", change)?;
    }
    writeln!(w)?;
    writeln!(w, "Rationale:\n  {}", view.rationale)?;
    writeln!(w)?;
    write!(
        w,
        "[y] apply, [n] discard, [d] show details, [o] open worktree: "
    )?;
    w.flush()?;
    Ok(())
}

pub(crate) fn read_decision(r: &mut dyn BufRead) -> Result<Decision> {
    let mut buf = String::new();
    r.read_line(&mut buf)?;
    Ok(match buf.trim().to_lowercase().as_str() {
        "y" | "yes" => Decision::Apply,
        "d" | "details" => Decision::ShowDetails,
        "o" | "open" => Decision::OpenWorktree,
        _ => Decision::Discard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_y_as_apply() {
        let mut r = Cursor::new(b"y\n");
        assert!(matches!(read_decision(&mut r).unwrap(), Decision::Apply));
    }

    #[test]
    fn unrecognized_defaults_to_discard() {
        let mut r = Cursor::new(b"\n");
        assert!(matches!(read_decision(&mut r).unwrap(), Decision::Discard));
    }

    #[test]
    fn render_includes_metric_and_rationale() {
        let view = GateView {
            skill: "demo",
            target_metric: "plan_quality",
            baseline_target: 0.66,
            best_target: 0.83,
            gated_snapshots: &[("other".into(), 1.0, 1.0)],
            old_body: "a\nb\n",
            new_body: "a\nc\n",
            rationale: "added scope-check",
        };
        let mut out = Vec::new();
        render(&view, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("plan_quality"));
        assert!(s.contains("added scope-check"));
        assert!(s.contains("+0.17"));
    }
}
