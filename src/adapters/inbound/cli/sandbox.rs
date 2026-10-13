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
//! | `tengu sandbox finalise --sandbox <n>` | the Finalise button without a browser: compile, load check, write `config.toml` (old one → `config.toml.prev`) |
//! | `tengu sandbox palette` | the builder palette as JSON (kinds, fields, wires, engines, secret stores, tools, skills) |

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;

use crate::application::builder::{create, BuilderError};
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
            let b = builder(&sandbox);
            let bp = b.state().map_err(refusal)?.blueprint;
            let p = b.preview(&bp).map_err(refusal)?;
            print!("{}", p.toml);
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
                eprintln!("{level}{at} {}", i.message);
            }
            for w in &p.load.warnings {
                eprintln!("warn  [loader] {w}");
            }
            eprintln!(
                "loader: {} · {} error(s), {} warning(s) · sha256 {}",
                if p.load.ok { "ok" } else { "refused" },
                p.status.counts.errors,
                p.status.counts.warnings,
                p.sha256
            );
            if !p.kept.is_empty() {
                eprintln!("kept from config.toml: {}", p.kept.join(", "));
            }
            if !p.status.ok || !p.load.ok {
                std::process::exit(1);
            }
            Ok(None)
        }
        SandboxAction::Finalise { sandbox } => {
            let b = builder(&sandbox);
            let bp = b.state().map_err(refusal)?.blueprint;
            let p = b.preview(&bp).map_err(refusal)?;
            let f = b.finalise(&bp, &p.sha256).map_err(refusal)?;
            eprintln!("Wrote {} (sha256 {})", f.written, f.sha256);
            if let Some(prev) = &f.backup {
                eprintln!("Previous file: {prev}");
            }
            for s in &f.secrets {
                eprintln!(
                    "  {:<8} {} ({}) used by {}",
                    s.state,
                    s.env,
                    s.backend,
                    s.used_by.join(", ")
                );
            }
            eprintln!("Next:");
            for n in &f.next {
                eprintln!("  {n}");
            }
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
