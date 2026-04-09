/// `rlean create-project <name>` — scaffold a new strategy project
///
/// Creates:
///   <name>/
///     config.json       — project metadata (language, parameters, local-id)
///     main.py           — QCAlgorithm strategy template
///     research.ipynb    — QuantBook Jupyter notebook
///     backtests/        — backtest results (populated by `rlean backtest`)
///     live/             — live results (populated by `rlean live`)
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::config::{ProjectConfig, WorkspaceConfig};

#[derive(clap::Args)]
pub struct CreateProjectArgs {
    /// Project name (becomes the directory name)
    pub name: String,

    /// Algorithm language [default: workspace default-language]
    #[arg(long, value_parser = ["python", "csharp"])]
    pub language: Option<String>,

    /// Short description of the strategy
    #[arg(long, default_value = "")]
    pub description: String,
}

pub fn run_create_project(args: CreateProjectArgs) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let project_dir = workspace.join(&args.name);

    if project_dir.exists() {
        bail!("Directory '{}' already exists.", project_dir.display());
    }

    // Resolve language: --language flag → lean.json default-language → error
    let language = if let Some(lang) = args.language {
        lang
    } else {
        WorkspaceConfig::load(&workspace)
            .ok()
            .map(|c| c.default_language)
            .ok_or_else(|| anyhow::anyhow!(
                "No --language specified and no lean.json found in {}.\n\
                 Run `rlean init` first or pass --language python|csharp.",
                workspace.display()
            ))?
    };

    // ── Directory skeleton ────────────────────────────────────────────────────
    std::fs::create_dir_all(&project_dir)?;
    std::fs::create_dir_all(project_dir.join("backtests"))?;
    std::fs::create_dir_all(project_dir.join("live"))?;

    // ── config.json ───────────────────────────────────────────────────────────
    let mut cfg = ProjectConfig::new(&language);
    cfg.description = args.description.clone();
    cfg.save(&project_dir)?;

    // ── main.py / Main.cs ─────────────────────────────────────────────────────
    match language.as_str() {
        "python" | _ => {
            let class_name = to_class_name(&args.name);
            let main_py = python_template(&class_name);
            std::fs::write(project_dir.join("main.py"), main_py)?;
        }
    }

    // ── research.ipynb ────────────────────────────────────────────────────────
    let notebook = research_notebook(&args.name);
    std::fs::write(project_dir.join("research.ipynb"), notebook)?;

    println!("Created project '{}'", args.name);
    println!("  {}/config.json", args.name);
    println!("  {}/main.py", args.name);
    println!("  {}/research.ipynb", args.name);
    println!("  {}/backtests/", args.name);
    println!("  {}/live/", args.name);
    println!();
    println!("Run backtest:   rlean backtest {}/main.py", args.name);
    println!("Open research:  rlean research {}", args.name);

    Ok(())
}

// ── Templates ─────────────────────────────────────────────────────────────────

/// Convert snake_case / kebab-case project name to PascalCase class name.
fn to_class_name(name: &str) -> String {
    name.split(|c: char| c == '_' || c == '-' || c == ' ')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect()
}

fn python_template(class_name: &str) -> String {
    format!(
        r#"# region imports
from AlgorithmImports import *
# endregion


class {class_name}(QCAlgorithm):

    def initialize(self):
        self.set_start_date(2020, 1, 1)
        self.set_end_date(2023, 1, 1)
        self.set_cash(100_000)

        self.spy = self.add_equity("SPY", Resolution.Daily).symbol

    def on_data(self, data: Slice):
        if not self.portfolio.invested:
            self.set_holdings(self.spy, 1)
"#,
        class_name = class_name
    )
}

fn research_notebook(project_name: &str) -> String {
    // Standard Jupyter nbformat v4 notebook with a single starter cell.
    let cell_source = format!(
        r#"from AlgorithmImports import *

qb = QuantBook()
qb.set_start_date(2020, 1, 1)
qb.set_end_date(2023, 1, 1)

# Add a security
spy = qb.add_equity("SPY", Resolution.Daily).symbol

# Pull daily history (last 252 bars)
history = qb.history(spy, 252, Resolution.Daily)
print(history.tail())"#
    );

    // Escape for JSON string (replace " with \", newline with \n)
    let cell_lines: Vec<String> = cell_source
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let sep = if i == 0 { "" } else { r"\n" };
            format!("\"{}{}\"", sep, line.replace('\"', "\\\""))
        })
        .collect();
    let source_json = cell_lines.join(",\n      ");

    format!(
        r#"{{
 "cells": [
  {{
   "cell_type": "code",
   "execution_count": null,
   "metadata": {{}},
   "outputs": [],
   "source": [
      {source}
   ]
  }}
 ],
 "metadata": {{
  "kernelspec": {{
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  }},
  "language_info": {{
   "name": "python",
   "version": "3.10.0"
  }}
 }},
 "nbformat": 4,
 "nbformat_minor": 5
}}
"#,
        source = source_json,
    )
}
