//! Skills that ship with the runtime (ADR-050 amendment, 2026-11-22).
//!
//! A bundled skill is the runtime's own know-how about this computer,
//! like the default roles: every peko sees it, and a release updates it.
//! It lives in the binary, not on disk, so nothing is copied into a
//! workspace and nothing goes stale there. A workspace skill of the same
//! name replaces it, so an owner can change one by writing their own.
//! `peko seed` and snapshots read only the workspace, so bundled skills
//! never travel with a peko.

/// One skill compiled into the runtime.
#[derive(Debug)]
pub struct BundledSkill {
    /// Invocation key, as a workspace skill's directory name would be.
    pub name: &'static str,
    /// The whole `SKILL.md`, frontmatter included.
    pub text: &'static str,
    /// Listed in the catalog only for runs acting for the owner: the
    /// skill needs tools (`Bash`) only those runs have.
    pub owner_only: bool,
}

/// Every bundled skill, sorted by name.
pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "coding-agents",
        text: include_str!("../../resources/skills/coding-agents/SKILL.md"),
        owner_only: true,
    },
    // ADR-123: using the owner's computer (`Computer`, owner runs only).
    BundledSkill {
        name: "computer-use",
        text: include_str!("../../resources/skills/computer-use/SKILL.md"),
        owner_only: true,
    },
    BundledSkill {
        name: "triage",
        text: include_str!("../../resources/skills/triage/SKILL.md"),
        owner_only: true,
    },
];

/// The bundled skill called `name`, if there is one.
#[must_use]
pub fn bundled_skill(name: &str) -> Option<&'static BundledSkill> {
    BUNDLED_SKILLS.iter().find(|skill| skill.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin::skill::{parse_yaml_frontmatter_typed, SkillFrontmatter};

    #[test]
    fn every_bundled_skill_parses_and_names_itself() {
        let mut previous = "";
        for skill in BUNDLED_SKILLS {
            let (frontmatter, body) =
                parse_yaml_frontmatter_typed::<SkillFrontmatter>(skill.text).unwrap();
            assert_eq!(frontmatter.name, skill.name);
            assert!(!frontmatter.description.trim().is_empty());
            assert!(!body.trim().is_empty());
            assert!(skill.name > previous, "BUNDLED_SKILLS must stay sorted");
            previous = skill.name;
        }
    }

    #[test]
    fn bundled_bodies_run_no_commands_and_take_no_arguments() {
        // A bundled skill is shown to every peko; it must not run shell
        // commands through dynamic context when invoked, and `$` would be
        // read as an argument placeholder.
        for skill in BUNDLED_SKILLS {
            assert!(
                !skill.text.contains("!`"),
                "{} has inline !`cmd`",
                skill.name
            );
            assert!(
                !skill.text.contains("```!"),
                "{} has a ```! block",
                skill.name
            );
            assert!(
                !skill.text.contains('$'),
                "{} has a $ placeholder",
                skill.name
            );
        }
    }
}
