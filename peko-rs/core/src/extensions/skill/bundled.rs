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
    /// skill needs tools (`Bash`) only those runs have. A catalog filter
    /// only; `Skill` does not check it (ADR-127 F2).
    pub owner_only: bool,
    /// Pages read with `Skill` `page` (ADR-127 D2), sorted by name.
    pub pages: &'static [BundledPage],
}

/// One page of a bundled skill: `resources/skills/<skill>/pages/<name>.md`,
/// frontmatter (`description`, `audience`) included.
#[derive(Debug)]
pub struct BundledPage {
    pub name: &'static str,
    pub text: &'static str,
}

/// Declare a bundled page by name; the file is beside the skill's
/// `SKILL.md`, under `pages/`.
macro_rules! page {
    ($skill:literal, $name:literal) => {
        BundledPage {
            name: $name,
            text: include_str!(concat!(
                "../../resources/skills/",
                $skill,
                "/pages/",
                $name,
                ".md"
            )),
        }
    };
}

/// Every bundled skill, sorted by name.
pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "coding-agents",
        text: include_str!("../../resources/skills/coding-agents/SKILL.md"),
        owner_only: true,
        pages: &[],
    },
    // ADR-123: using the owner's computer (`Computer`, owner runs only).
    BundledSkill {
        name: "computer-use",
        text: include_str!("../../resources/skills/computer-use/SKILL.md"),
        owner_only: true,
        pages: &[],
    },
    // ADR-127: the peko's own manual. Listed for every run; each page
    // says whom it is for, and a run's map lists only its own.
    BundledSkill {
        name: "peko",
        text: include_str!("../../resources/skills/peko/SKILL.md"),
        owner_only: false,
        pages: &[
            page!("peko", "changing-yourself"),
            page!("peko", "how-you-work"),
            page!("peko", "improving-peko"),
            page!("peko", "operating-peko"),
            page!("peko", "seeds"),
            page!("peko", "standing"),
        ],
    },
    BundledSkill {
        name: "triage",
        text: include_str!("../../resources/skills/triage/SKILL.md"),
        owner_only: true,
        pages: &[],
    },
];

/// The bundled skill called `name`, if there is one.
#[must_use]
pub fn bundled_skill(name: &str) -> Option<&'static BundledSkill> {
    BUNDLED_SKILLS.iter().find(|skill| skill.name == name)
}

impl BundledSkill {
    /// The page called `name`, if this skill has one.
    #[must_use]
    pub fn page(&self, name: &str) -> Option<&'static BundledPage> {
        self.pages.iter().find(|page| page.name == name)
    }
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
        // read as an argument placeholder. Pages are plain text, held to
        // the same rule so a page can later be rendered like a body.
        for skill in BUNDLED_SKILLS {
            let texts = std::iter::once((skill.name, skill.text))
                .chain(skill.pages.iter().map(|page| (page.name, page.text)));
            for (name, text) in texts {
                assert!(
                    !text.contains("!`"),
                    "{}/{name} has inline !`cmd`",
                    skill.name
                );
                assert!(
                    !text.contains("```!"),
                    "{}/{name} has a ```! block",
                    skill.name
                );
                assert!(
                    !text.contains('$'),
                    "{}/{name} has a $ placeholder",
                    skill.name
                );
            }
        }
    }

    /// ADR-127 D9: every page parses, says whom it is for, and fits.
    #[test]
    fn every_bundled_page_parses_declares_its_audience_and_fits() {
        use crate::tools::builtin::skill::{PageFrontmatter, MAX_SKILL_PAGE_BYTES};
        for skill in BUNDLED_SKILLS {
            let mut previous = "";
            for page in skill.pages {
                assert!(
                    page.name > previous,
                    "{} pages must stay sorted",
                    skill.name
                );
                previous = page.name;
                assert!(
                    crate::tools::builtin::skill::is_page_name(page.name),
                    "{}",
                    page.name
                );
                assert!(
                    page.text.len() <= MAX_SKILL_PAGE_BYTES,
                    "{}/{} is {} bytes",
                    skill.name,
                    page.name,
                    page.text.len()
                );
                let (yaml, _) =
                    crate::tools::builtin::skill::parse_yaml_frontmatter(page.text).unwrap();
                assert!(
                    yaml.lines().any(|l| l.starts_with("audience: ")),
                    "{}/{} must declare its audience",
                    skill.name,
                    page.name
                );
                let (frontmatter, body) =
                    parse_yaml_frontmatter_typed::<PageFrontmatter>(page.text).unwrap();
                assert!(!frontmatter.description.trim().is_empty());
                assert!(!body.trim().is_empty());
            }
        }
    }

    /// ADR-127 D3: the manual is for every run, and the pages its map
    /// names exist.
    #[test]
    fn the_peko_skill_is_for_every_run_and_has_the_six_pages() {
        let peko = bundled_skill("peko").unwrap();
        assert!(!peko.owner_only);
        let names: Vec<&str> = peko.pages.iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "changing-yourself",
                "how-you-work",
                "improving-peko",
                "operating-peko",
                "seeds",
                "standing"
            ]
        );
        for named in backticked(peko.text)
            .into_iter()
            .chain(peko.pages.iter().flat_map(|p| backticked(p.text)))
        {
            if named.contains('-') && !named.contains(' ') && named == named.to_lowercase() {
                if let Some(page) = names.iter().find(|n| **n == named) {
                    assert!(peko.page(page).is_some());
                }
            }
        }
    }

    /// The text of every `` `…` `` span in `text`.
    pub(crate) fn backticked(text: &str) -> Vec<String> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    /// ADR-127 D9: every `peko …` command a page names is in the
    /// command-line reference, which is generated from the parser
    /// (`scripts/gen_cli_reference.py`). The CLI crate checks the same
    /// against the parser itself.
    #[test]
    fn every_command_the_manual_names_is_in_the_cli_reference() {
        let reference = include_str!("../../../../../docs/user-guide/CLI_REFERENCE.md");
        let known: Vec<&str> = reference
            .lines()
            .filter_map(|line| {
                line.strip_prefix("## `")
                    .or_else(|| line.strip_prefix("### `"))
            })
            .filter_map(|line| line.strip_suffix('`'))
            .collect();
        let mut checked = 0;
        for skill in BUNDLED_SKILLS {
            let texts = std::iter::once(skill.text).chain(skill.pages.iter().map(|p| p.text));
            for text in texts {
                for command in backticked(text) {
                    let Some(words) = manual_command(&command) else {
                        continue;
                    };
                    // Hidden commands are not in the reference; the CLI
                    // crate's test checks them against the parser.
                    if HIDDEN_COMMANDS.contains(&words[0].as_str()) {
                        continue;
                    }
                    // The longest prefix of the words that the reference
                    // has a section for must cover every bare word.
                    let mut matched = 0;
                    for n in 1..=words.len() {
                        if known.contains(&format!("peko {}", words[..n].join(" ")).as_str()) {
                            matched = n;
                        }
                    }
                    assert!(
                        matched > 0,
                        "{}: `{command}` names no command in CLI_REFERENCE.md",
                        skill.name
                    );
                    // A word after a command that has subcommands must be
                    // one of them (`peko device pair` is not a command).
                    let found = format!("peko {} ", words[..matched].join(" "));
                    assert!(
                        matched == words.len() || !known.iter().any(|k| k.starts_with(&found)),
                        "{}: `{command}`: `{}` is not a subcommand of `{}`",
                        skill.name,
                        words[matched],
                        found.trim_end()
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 20, "only {checked} commands found");
    }

    /// Top-level commands `peko --help` does not list (`hide = true`).
    const HIDDEN_COMMANDS: &[&str] = &["config", "runtime", "tunnel", "vault"];

    /// The command words of a `peko …` span: up to the first argument,
    /// placeholder or flag. `None` when the span is not a command.
    pub(crate) fn manual_command(span: &str) -> Option<Vec<String>> {
        let rest = span.strip_prefix("peko ")?;
        let words: Vec<String> = rest
            .split_whitespace()
            .take_while(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
            .take_while(|w| !w.starts_with('-'))
            .map(str::to_string)
            .collect();
        (!words.is_empty()).then_some(words)
    }
}
