//! SKILL.md loading (docs/16 §skills, M16.1).
//!
//! > "SKILL.md-compatible on purpose — the existing ecosystem should port with
//! > zero edits: YAML frontmatter (`name`, `description`, trigger hints) +
//! > markdown body + optional `references/` loaded on demand."
//!
//! "Zero edits" is the acceptance criterion, and it shapes every decision here:
//! unknown frontmatter keys are **kept, not rejected**, because a skill written
//! for another runtime will carry keys we have never heard of and refusing it
//! would break the compatibility this is for.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default token budget for a skill body (docs/16: "default 2k").
pub const DEFAULT_BODY_TOKEN_BUDGET: u32 = 2_000;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill io: {0}")]
    Io(String),
    #[error("{path}: missing YAML frontmatter (a SKILL.md must open with `---`)")]
    NoFrontmatter { path: String },
    #[error("{path}: unterminated frontmatter (no closing `---`)")]
    UnterminatedFrontmatter { path: String },
    #[error("{path}: frontmatter is not valid YAML: {detail}")]
    BadYaml { path: String, detail: String },
    #[error("{path}: frontmatter needs a non-empty `name`")]
    MissingName { path: String },
    #[error("{path}: frontmatter needs a non-empty `description` — it is what the model sees in the index")]
    MissingDescription { path: String },
}

/// The frontmatter block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    /// One or two lines. This is what rides in the stable prefix, so it is the
    /// only thing the model knows about an unloaded skill.
    pub description: String,
    /// Hints that a trigger classifier may use (docs/16).
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Everything else, preserved verbatim.
    ///
    /// Rejecting unknown keys would break "port with zero edits" the first time
    /// a skill written for another runtime declared something we do not model.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_yaml_ng::Value>,
}

/// A loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    pub path: PathBuf,
    /// `references/` beside the SKILL.md, loaded on demand — never eagerly.
    pub references: Vec<PathBuf>,
}

impl Skill {
    /// The one-line form that lives in the stable prefix.
    ///
    /// docs/16: the index is "name + description, ~1-2 lines each". Kept short
    /// deliberately: this text is in the cached prefix for every turn of the
    /// session, so its cost is paid on every request.
    pub fn index_entry(&self) -> String {
        format!(
            "{}: {}",
            self.frontmatter.name,
            self.frontmatter.description.trim()
        )
    }

    pub fn approx_body_tokens(&self) -> u32 {
        (self.body.len() as u32).div_ceil(4)
    }

    /// True when the body exceeds the budget and should have its tail spilled.
    ///
    /// docs/16: "oversized skills get their tail spilled to artifacts with
    /// expand-on-demand". Reported rather than truncated here, so the caller
    /// that owns an artifact store decides.
    pub fn exceeds_budget(&self, budget: u32) -> bool {
        self.approx_body_tokens() > budget
    }

    /// Parse SKILL.md content.
    pub fn parse(path: &Path, raw: &str) -> Result<Self, SkillError> {
        let display = path.display().to_string();

        // Tolerate a leading BOM and blank lines: a file exported from an
        // editor may have either, and neither is a reason to reject a skill.
        let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
        let Some(rest) = trimmed.strip_prefix("---") else {
            return Err(SkillError::NoFrontmatter { path: display });
        };
        let rest = rest.trim_start_matches(['\r', '\n']);

        // The closing fence must be at the start of a line, or a `---`
        // horizontal rule inside the frontmatter would end it early.
        let end = rest
            .lines()
            .scan(0usize, |offset, line| {
                let at = *offset;
                *offset += line.len() + 1;
                Some((at, line))
            })
            .find(|(_, line)| line.trim_end() == "---")
            .map(|(at, _)| at);

        let Some(end) = end else {
            return Err(SkillError::UnterminatedFrontmatter { path: display });
        };

        let yaml = &rest[..end];
        let body = rest[end..]
            .trim_start_matches("---")
            .trim_start_matches(['\r', '\n'])
            .to_string();

        let frontmatter: SkillFrontmatter =
            serde_yaml_ng::from_str(yaml).map_err(|e| SkillError::BadYaml {
                path: display.clone(),
                detail: e.to_string(),
            })?;

        if frontmatter.name.trim().is_empty() {
            return Err(SkillError::MissingName { path: display });
        }
        if frontmatter.description.trim().is_empty() {
            return Err(SkillError::MissingDescription { path: display });
        }

        Ok(Skill {
            frontmatter,
            body,
            path: path.to_path_buf(),
            references: Vec::new(),
        })
    }

    /// Load a SKILL.md from disk, discovering `references/` beside it.
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| SkillError::Io(format!("{}: {e}", path.display())))?;
        let mut skill = Self::parse(path, &raw)?;

        if let Some(dir) = path.parent() {
            let refs = dir.join("references");
            if refs.is_dir() {
                let mut found: Vec<PathBuf> = std::fs::read_dir(&refs)
                    .map_err(|e| SkillError::Io(format!("{}: {e}", refs.display())))?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect();
                // Deterministic order, so an index is reproducible.
                found.sort();
                skill.references = found;
            }
        }
        Ok(skill)
    }
}

/// Discover every SKILL.md under a directory.
///
/// Both layouts the ecosystem uses are accepted: `skills/<name>/SKILL.md` and a
/// bare `skills/<name>.md`. Supporting only the first would reject half the
/// skills in circulation for no benefit.
pub fn discover(root: &Path) -> Result<Vec<Skill>, SkillError> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| SkillError::Io(format!("{}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `references/` holds supporting material, not more skills.
                if path.file_name().is_some_and(|n| n == "references") {
                    continue;
                }
                stack.push(path);
            } else if is_skill_file(&path) {
                out.push(Skill::load(&path)?);
            }
        }
    }
    out.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
    Ok(out)
}

fn is_skill_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.eq_ignore_ascii_case("SKILL.md")
        || (name.ends_with(".md") && !name.eq_ignore_ascii_case("README.md"))
}

/// The index that rides in the stable prefix.
pub fn index(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Skills\n");
    for s in skills {
        out.push_str(&format!("- {}\n", s.index_entry()));
    }
    out
}
