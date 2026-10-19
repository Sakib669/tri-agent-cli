//! Agent skills: folders containing a SKILL.md (YAML frontmatter with name +
//! description, then instruction markdown). Discovered from configured dirs,
//! injected as a compact index into the system prompt, and loadable with
//! `/<skill-name>` or `/skill <name>`.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

impl Skill {
    pub fn body(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.dir.join("SKILL.md")).ok()?;
        Some(strip_frontmatter(&raw))
    }

    pub fn short_desc(&self, max: usize) -> String {
        let d = self.description.replace('\n', " ");
        let d = d.split_whitespace().collect::<Vec<_>>().join(" ");
        if d.chars().count() <= max {
            d
        } else {
            let mut out: String = d.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }
}

fn strip_frontmatter(raw: &str) -> String {
    if raw.starts_with("---") {
        if let Some(end) = raw.find("\n---") {
            return raw[end + 4..].trim_start().to_string();
        }
    }
    raw.to_string()
}

fn parse_frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    if !raw.starts_with("---") {
        return (None, None);
    }
    let Some(end) = raw.find("\n---") else {
        return (None, None);
    };
    let fm = &raw[3..end];
    let mut name = None;
    let mut desc = None;
    let mut desc_lines: Vec<String> = Vec::new();
    let mut in_desc = false;
    for line in fm.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
            in_desc = false;
        } else if let Some(rest) = trimmed.strip_prefix("description:") {
            let rest = rest.trim();
            let first = rest.trim_start_matches('>').trim_start_matches('|').trim();
            if !first.is_empty() {
                desc_lines.push(first.trim_matches('"').to_string());
            }
            in_desc = true;
        } else if in_desc && !trimmed.is_empty() && !trimmed.contains(':') {
            desc_lines.push(trimmed.to_string());
        } else if trimmed.contains(':') && !trimmed.starts_with('-') {
            in_desc = false;
        }
    }
    if !desc_lines.is_empty() {
        desc = Some(desc_lines.join(" "));
    }
    (name, desc)
}

/// Walk a directory tree (bounded depth) for SKILL.md files.
fn walk_skills(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut has_skill = false;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_skills(&p, depth + 1, out);
        } else if p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
            has_skill = true;
        }
    }
    if has_skill {
        out.push(dir.to_path_buf());
    }
}

/// Discover skills from the given roots; later roots win on duplicate names.
pub fn discover(roots: &[PathBuf]) -> Vec<Skill> {
    let mut found = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut dirs = Vec::new();
        walk_skills(root, 0, &mut dirs);
        for dir in dirs {
            let Ok(raw) = std::fs::read_to_string(dir.join("SKILL.md")) else {
                continue;
            };
            let (fm_name, fm_desc) = parse_frontmatter(&raw);
            let name = fm_name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
                dir.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            let description = fm_desc.unwrap_or_default();
            found.push(Skill {
                name,
                description,
                dir,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Compact index lines for the system prompt.
pub fn prompt_index(skills: &[Skill], max_chars: usize) -> String {
    let mut out = String::from(
        "## Available skills\nYou can load a skill by running its slash command (e.g. `/ponytail`). \
         Skills contain detailed instructions for specific tasks; load one when the user's request matches it.\n",
    );
    for s in skills {
        let line = format!("- /{}: {}\n", s.name, s.short_desc(70));
        if out.chars().count() + line.chars().count() > max_chars {
            out.push_str("- (more skills: run /skills)\n");
            break;
        }
        out.push_str(&line);
    }
    out
}
