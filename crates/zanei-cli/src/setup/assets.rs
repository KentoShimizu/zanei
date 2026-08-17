pub const SKILL: &str = include_str!("../../../../skills/SKILL.md");

const FRONTMATTER_START: &str = "---\n";
const FRONTMATTER_END: &str = "\n---\n";

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SkillAssetError {
    #[error("frontmatter must be enclosed by `---` delimiters")]
    InvalidFrontmatter,
    #[error("the skill body must begin with a level-one Markdown heading")]
    MissingTitle,
}

pub fn derive_instruction_document(
    skill: &str,
    replacement_title: &str,
) -> Result<String, SkillAssetError> {
    let frontmatter = skill
        .strip_prefix(FRONTMATTER_START)
        .ok_or(SkillAssetError::InvalidFrontmatter)?;
    let (_, body) = frontmatter
        .split_once(FRONTMATTER_END)
        .ok_or(SkillAssetError::InvalidFrontmatter)?;
    let body = body.trim_start_matches('\n');
    let (title, instructions) = body.split_once('\n').ok_or(SkillAssetError::MissingTitle)?;
    if !title.starts_with("# ") {
        return Err(SkillAssetError::MissingTitle);
    }

    Ok(format!("{replacement_title}\n{instructions}"))
}

#[cfg(test)]
mod tests {
    use super::{SKILL, derive_instruction_document};

    #[test]
    fn derives_agent_document_by_removing_frontmatter_and_replacing_title() {
        let derived = derive_instruction_document(SKILL, "## Zanei activity context")
            .expect("valid embedded skill");

        assert!(derived.starts_with(
            "## Zanei activity context\n\nZanei records the user's activity on this machine"
        ));
        assert!(!derived.contains("name: zanei"));
        assert!(!derived.contains("# Zanei\n"));
        assert!(derived.ends_with("the privacy model.\n"));
    }
}
