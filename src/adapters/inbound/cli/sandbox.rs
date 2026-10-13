//! `tengu sandbox` — make and compose sandboxes with the Studio builder
//! (`docs/studio-builder-2026-10-10.md`). The same use cases as the page
//! (`application/builder.rs`), so every step can be run and debugged
//! without a browser. Messages go to stderr; `compile` / `palette` print
//! their result on stdout.
//!
//! | Command | Does |
//! |---|---|
//! | `tengu sandbox new [--name <n>] [--template blank\|team\|telegram] [--no-open] [--port <p>]` | asks the name (and the template) when not given, writes `sandboxes/<n>/builder.json` + `config.toml` (checked by the real loader first; an existing dir is refused), then opens the builder: `tengu studio --sandbox <n> --allow-edit --open` |
//! | `tengu sandbox compile --sandbox <n>` | `builder.json` → the TOML Finalise would write (stdout); every issue + the loader's verdict on stderr; exit 1 on an error. Writes nothing |
//! | `tengu sandbox finalise --sandbox <n>` | the Finalise button without a browser: compile, load check, then on stderr every issue, the loader's warnings and the diff vs `config.toml` — then the write (old one → `config.toml.prev`). Non-interactive; refused when view-only (a `config.toml` that does not parse too) |
//! | `tengu sandbox palette` | the builder palette as JSON (kinds, fields, wires, engines, secret stores, tools, skills) |
//!
//! `--name` / `--sandbox` pass the builder's name rule
//! (`config::builder::sandbox_name_error`: no `/`, `.`, absolute path)
//! before anything is read; the store checks it again.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;

use crate::application::builder::{create, Builder, BuilderError, Preview};
use crate::bootstrap::builder::{builder, drafts, facts, SANDBOXES_DIR};
use crate::config::builder::palette::palette;
use crate::config::builder::sandbox_name_error;
use crate::config::builder::template::TEMPLATES;
use crate::domain::blueprint::Level;

#[derive(Subcommand)]
pub(super) enum SandboxAction {
    /// Create sandboxes/<name>/ from a template (asks the name), then open
    /// the drag-and-drop builder in the browser.
    New {
        /// Sandbox name (asked when omitted): a-z 0-9 - _, ≤ 40.
        #[arg(long)]
        name: Option<String>,
        /// blank · team · telegram (asked on a terminal when omitted; else blank).
        #[arg(long)]
        template: Option<String>,
        /// Only create the files; do not start the builder.
        #[arg(long)]
        no_open: bool,
        /// Builder port (default: any free one).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print the config.toml the canvas compiles to; issues on stderr.
    Compile {
        #[arg(long)]
        sandbox: String,
    },
    /// Validate the canvas and write config.toml (Finalise without a browser).
    Finalise {
        #[arg(long)]
        sandbox: String,
    },
    /// Print the builder palette as JSON.
    Palette,
}

/// What `new` hands back to start the builder server.
pub(super) struct Open {
    pub name: String,
    pub port: Option<u16>,
}

fn ask(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        bail!("no answer (stdin closed) — pass --name");
    }
    Ok(line.trim().to_string())
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The name from `--name` or asked (three tries), new and valid.
fn sandbox_name(given: Option<String>) -> Result<String> {
    let root = PathBuf::from(SANDBOXES_DIR);
    let check = |n: &str| -> Option<String> {
        sandbox_name_error(n).or_else(|| {
            root.join(n)
                .exists()
                .then(|| format!("sandboxes/{n} already exists — pick another name"))
        })
    };
    if let Some(n) = given {
        return match check(&n) {
            None => Ok(n),
            Some(why) => Err(anyhow!(why)),
        };
    }
    for _ in 0..3 {
        let n = ask("Sandbox name (a-z 0-9 - _): ")?;
        match check(&n) {
            None => return Ok(n),
            Some(why) => eprintln!("  {why}"),
        }
    }
    bail!("no valid sandbox name after three tries")
}

fn template_choice(given: Option<String>) -> Result<String> {
    if let Some(t) = given {
        return Ok(t);
    }
    if !std::io::stdin().is_terminal() {
        return Ok("blank".into());
    }
    eprintln!("Start from:");
    for (i, (name, what)) in TEMPLATES.iter().enumerate() {
        eprintln!("  {}. {name:<9} {what}", i + 1);
    }
    let a = ask("Template [1]: ")?;
    if a.is_empty() {
        return Ok(TEMPLATES[0].0.into());
    }
    if let Ok(i) = a.parse::<usize>() {
        if let Some((name, _)) = TEMPLATES.get(i.wrapping_sub(1)) {
            return Ok((*name).into());
        }
    }
    Ok(a)
}

fn refusal(e: BuilderError) -> anyhow::Error {
    anyhow!("{e}")
}

/// The builder of `--sandbox <n>`: the name rule first (no `/`, `.`,
/// absolute path), before anything is read.
fn open_builder(sandbox: &str) -> Result<Builder> {
    if let Some(why) = sandbox_name_error(sandbox) {
        bail!("--sandbox: sandbox name {why}");
    }
    builder(sandbox).map_err(refusal)
}

/// Every issue of the preview (`[card <id> · <field>]`, `[wire <id>]`), the
/// loader's warnings and verdict, the kept sections.
fn report(p: &Preview, out: &mut dyn Write) -> std::io::Result<()> {
    for i in &p.status.issues {
        let level = match i.level {
            Level::Error => "error",
            Level::Warn => "warn ",
        };
        let at = match (&i.node, &i.edge, &i.field) {
            (Some(n), _, Some(f)) => format!(" [card {n} · {f}]"),
            (Some(n), _, None) => format!(" [card {n}]"),
            (None, Some(e), _) => format!(" [wire {e}]"),
            _ => String::new(),
        };
        writeln!(out, "{level}{at} {}", i.message)?;
    }
    for w in &p.load.warnings {
        writeln!(out, "warn  [loader] {w}")?;
    }
    writeln!(
        out,
        "loader: {} · {} error(s), {} warning(s) · sha256 {}",
        if p.load.ok { "ok" } else { "refused" },
        p.status.counts.errors,
        p.status.counts.warnings,
        p.sha256
    )?;
    if !p.kept.is_empty() {
        writeln!(out, "kept from config.toml: {}", p.kept.join(", "))?;
    }
    Ok(())
}

/// Finalise without a browser: the preview's issues, warnings and diff
/// first, then the write (`Builder::finalise`: refused when view-only — a
/// `config.toml` that does not parse too —, stale, running or invalid).
fn finalise(b: &Builder, out: &mut dyn Write) -> Result<()> {
    let bp = b.state().map_err(refusal)?.blueprint;
    let p = b.preview(&bp).map_err(refusal)?;
    report(&p, out)?;
    match &p.why_not {
        Some(why) => writeln!(out, "view-only: {why}")?,
        None if p.diff.is_empty() => writeln!(out, "diff: no change to config.toml")?,
        None => write!(out, "{}", p.diff)?,
    }
    let f = b.finalise(&bp, &p.sha256).map_err(refusal)?;
    writeln!(out, "Wrote {} (sha256 {})", f.written, f.sha256)?;
    if let Some(prev) = &f.backup {
        writeln!(out, "Previous file: {prev}")?;
    }
    for s in &f.secrets {
        writeln!(
            out,
            "  {:<8} {} ({}) used by {}",
            s.state,
            s.env,
            s.backend,
            s.used_by.join(", ")
        )?;
    }
    writeln!(out, "Next:")?;
    for n in &f.next {
        writeln!(out, "  {n}")?;
    }
    Ok(())
}

/// Runs the command; `Some(Open)` = start the builder server next.
pub(super) fn run_sandbox(action: SandboxAction) -> Result<Option<Open>> {
    match action {
        SandboxAction::New {
            name,
            template,
            no_open,
            port,
        } => {
            let name = sandbox_name(name)?;
            let tpl = template_choice(template)?;
            let store = drafts(SANDBOXES_DIR);
            let dir = create(store.as_ref(), &facts(&cwd()), &name, &tpl).map_err(refusal)?;
            eprintln!(
                "Created {0}/ (template {tpl}):\n  {0}/builder.json   the canvas\n  {0}/config.toml    what tengu loads (Finalise rewrites it)",
                dir.display()
            );
            if no_open {
                eprintln!("Open the builder: tengu studio --sandbox {name} --allow-edit");
                return Ok(None);
            }
            eprintln!("Starting the builder (Ctrl-C stops it) …");
            Ok(Some(Open { name, port }))
        }
        SandboxAction::Compile { sandbox } => {
            let b = open_builder(&sandbox)?;
            let bp = b.state().map_err(refusal)?.blueprint;
            let p = b.preview(&bp).map_err(refusal)?;
            print!("{}", p.toml);
            report(&p, &mut std::io::stderr())?;
            if !p.status.ok || !p.load.ok {
                std::process::exit(1);
            }
            Ok(None)
        }
        SandboxAction::Finalise { sandbox } => {
            finalise(&open_builder(&sandbox)?, &mut std::io::stderr())?;
            Ok(None)
        }
        SandboxAction::Palette => {
            println!(
                "{}",
                serde_json::to_string_pretty(&palette(&facts(&cwd())))?
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::adapters::outbound::builder_store::FsDrafts;
    use crate::config::builder::Facts;

    const TRAVERSAL: &[&str] = &["../x", "a/../../b", "a/b", "/tmp/x", "..", ""];

    /// The builder of `demo` (blank template) under `<root>/sandboxes`.
    fn demo(root: &Path) -> (Builder, PathBuf) {
        let store = Arc::new(FsDrafts::new(root.join("sandboxes"), Box::new(|_| None)));
        let dir = create(store.as_ref(), &Facts::default(), "demo", "blank").unwrap();
        (Builder::new("demo", Facts::default(), store).unwrap(), dir)
    }

    #[test]
    fn traversal_names_are_refused_before_anything_is_read() {
        for bad in TRAVERSAL {
            for action in [
                SandboxAction::Compile {
                    sandbox: bad.to_string(),
                },
                SandboxAction::Finalise {
                    sandbox: bad.to_string(),
                },
                SandboxAction::New {
                    name: Some(bad.to_string()),
                    template: Some("blank".into()),
                    no_open: true,
                    port: None,
                },
            ] {
                let err = run_sandbox(action).err().expect("refused");
                assert!(err.to_string().contains("use lower-case"), "{bad}: {err:#}");
            }
        }
    }

    #[test]
    fn finalise_shows_warnings_and_the_diff_before_it_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let (b, dir) = demo(tmp.path());
        let mut bp = b.state().unwrap().blueprint;
        let agent = bp.nodes.iter_mut().find(|n| n.kind == "agent").unwrap();
        agent
            .fields
            .insert("model".into(), serde_json::json!("openai/gpt-5"));
        agent.fields.insert("zzz".into(), serde_json::json!(1));
        b.save(&bp).unwrap();
        let mut out = Vec::new();
        finalise(&b, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle}:\n{out}"))
        };
        let (warn, diff) = (at("unknown field 'zzz'"), at("--- config.toml (on disk)"));
        let (plus, wrote) = (at("+model = \"openai/gpt-5\""), at("Wrote "));
        assert!(warn < diff && diff < plus && plus < wrote, "{out}");
        assert!(out.contains("Previous file: "), "{out}");
        let written = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(written.contains("model = \"openai/gpt-5\""), "{written}");
    }

    #[test]
    fn finalise_leaves_a_config_that_does_not_parse_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let (b, dir) = demo(tmp.path());
        let broken = "[agents.assistant\nengine = \"openrouter\"\n# operator notes ✓\n";
        std::fs::write(dir.join("config.toml"), broken).unwrap();
        let mut out = Vec::new();
        let err = finalise(&b, &mut out).unwrap_err();
        assert!(err.to_string().contains("does not parse"), "{err:#}");
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("view-only: ") && !out.contains("Wrote "),
            "{out}"
        );
        assert_eq!(
            std::fs::read(dir.join("config.toml")).unwrap(),
            broken.as_bytes()
        );
        assert!(!dir.join("config.toml.prev").exists());
    }
}
