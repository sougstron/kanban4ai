//! Strict YAML form for agent-authored questions.
//!
//! Agents rarely populate the free-form `ask --variants` structure, so the
//! human answer panel ends up with a wall of text and no selectable options.
//! An [`AskForm`] fixes that at the source: the agent writes a small YAML file
//! following a documented schema and submits it with `kanban ask-form`, which
//! parses and validates it and posts one `question` message per entry. Each
//! question's `options` map onto the existing `Message::variants`, so nothing
//! in the on-disk format or the fixtures changes.
//!
//! ```yaml
//! questions:
//!   - id: q1                 # optional, agent-facing label (traceability only)
//!     prompt: Which auth backend should this use?   # required, non-empty
//!     options:               # optional; become the message `variants`
//!       - OAuth2
//!       - API key
//!     allow_custom: true     # optional, default true
//! ```

use serde::Deserialize;

use crate::core::error::{KanbanError, Result};

/// Hint appended to a question body when the agent forbids a free-text answer.
const PICK_ONE_HINT: &str = "Pick one of the listed options.";

/// A parsed `ask-form` document: one or more questions to post at once.
#[derive(Debug, Clone, Deserialize)]
pub struct AskForm {
    #[serde(default)]
    pub questions: Vec<FormQuestion>,
}

/// A single form entry. `id` is an optional agent-facing label kept only for
/// readability; `options` become the posted message's `variants`.
#[derive(Debug, Clone, Deserialize)]
pub struct FormQuestion {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub allow_custom: Option<bool>,
}

impl AskForm {
    /// Parse and validate a form document. YAML syntax errors surface as
    /// [`KanbanError::Yaml`]; semantic problems (no questions, blank prompt) as
    /// [`KanbanError::Invalid`] with a message that names the offending entry.
    /// Blank options are trimmed away so a sloppy `- ` never becomes a variant.
    pub fn parse(text: &str) -> Result<AskForm> {
        let mut form: AskForm = serde_yaml_ng::from_str(text)?;

        if form.questions.is_empty() {
            return Err(KanbanError::Invalid(
                "ask-form: `questions` must contain at least one entry".to_string(),
            ));
        }

        for (index, question) in form.questions.iter_mut().enumerate() {
            if question.prompt.trim().is_empty() {
                return Err(KanbanError::Invalid(format!(
                    "ask-form: question {} has an empty `prompt`",
                    index + 1
                )));
            }
            question.prompt = question.prompt.trim().to_string();
            question.options.retain(|option| !option.trim().is_empty());
            for option in &mut question.options {
                *option = option.trim().to_string();
            }
        }

        Ok(form)
    }
}

impl FormQuestion {
    /// The message body: the prompt, plus a "pick one" hint when the agent set
    /// `allow_custom: false` and supplied options. Enforcement of that hint is
    /// advisory in the TUI for now — the hint keeps the intent visible to both
    /// the human and the agent.
    pub fn body(&self) -> String {
        if self.allow_custom == Some(false) && !self.options.is_empty() {
            format!("{}\n({PICK_ONE_HINT})", self.prompt)
        } else {
            self.prompt.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_questions_with_options() {
        let form = AskForm::parse(
            "questions:\n  - id: q1\n    prompt: Which backend?\n    options:\n      - OAuth2\n      - API key\n  - prompt: Any constraints?\n",
        )
        .unwrap();
        assert_eq!(form.questions.len(), 2);
        assert_eq!(form.questions[0].prompt, "Which backend?");
        assert_eq!(form.questions[0].options, vec!["OAuth2", "API key"]);
        assert_eq!(form.questions[0].id.as_deref(), Some("q1"));
        assert!(form.questions[1].options.is_empty());
    }

    #[test]
    fn empty_questions_is_rejected() {
        assert!(matches!(
            AskForm::parse("questions: []\n"),
            Err(KanbanError::Invalid(_))
        ));
        assert!(matches!(
            AskForm::parse("{}\n"),
            Err(KanbanError::Invalid(_))
        ));
    }

    #[test]
    fn blank_prompt_is_rejected() {
        assert!(matches!(
            AskForm::parse("questions:\n  - prompt: '   '\n"),
            Err(KanbanError::Invalid(_))
        ));
        assert!(matches!(
            AskForm::parse("questions:\n  - options: [a, b]\n"),
            Err(KanbanError::Invalid(_))
        ));
    }

    #[test]
    fn malformed_yaml_is_a_yaml_error() {
        assert!(matches!(
            AskForm::parse("questions: [: :\n"),
            Err(KanbanError::Yaml(_))
        ));
    }

    #[test]
    fn allow_custom_defaults_to_none_and_blank_options_are_stripped() {
        let form =
            AskForm::parse("questions:\n  - prompt: Pick\n    options:\n      - a\n      - '  '\n")
                .unwrap();
        assert_eq!(form.questions[0].allow_custom, None);
        assert_eq!(form.questions[0].options, vec!["a"]);
        assert_eq!(form.questions[0].body(), "Pick");
    }

    #[test]
    fn allow_custom_false_appends_hint_when_options_present() {
        let form = AskForm::parse(
            "questions:\n  - prompt: Pick one\n    allow_custom: false\n    options: [a, b]\n",
        )
        .unwrap();
        assert_eq!(
            form.questions[0].body(),
            "Pick one\n(Pick one of the listed options.)"
        );

        // No options → nothing to lock to, so no hint even when custom is off.
        let no_opts =
            AskForm::parse("questions:\n  - prompt: Free\n    allow_custom: false\n").unwrap();
        assert_eq!(no_opts.questions[0].body(), "Free");
    }
}
