//! `Ask` — ask questions that wait for an answer (ADR-114 D2, D4).
//!
//! In a conversation, the questions ride the run's reply as a card the
//! person answers in one go ([`crate::principal::reply_options`]); the
//! answer is the person's next message. In a delegated run, they go back
//! to the run that delegated the task with its result
//! ([`crate::agents::delegated_asks`]), which answers by continuing the
//! session. Nothing waits in either case, and questions carry no
//! authority: an answer is only someone's message.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use peko_channel::{
    ChannelId, ChannelPort, MAX_OPTIONS, MAX_OPTION_CHARS, MAX_QUESTIONS, MAX_QUESTION_CHARS,
};
use peko_protocol::channel::{Ask, Question};
use peko_subject::{ActorRef, PrincipalId};
use peko_tools_core::{Tool, ToolContext};
use serde_json::json;

/// Wire name registered with the ToolingRuntime.
pub const ASK_TOOL_NAME: &str = "Ask";

/// Ask questions with the run's reply, or of the run that delegated it.
pub struct AskTool {
    port: Arc<dyn ChannelPort>,
}

impl AskTool {
    #[must_use]
    pub fn new(port: Arc<dyn ChannelPort>) -> Self {
        Self { port }
    }

    /// `for` must name a person in the conversation; returns their
    /// canonical address.
    async fn resolve_for(&self, channel: &ChannelId, given: &str) -> anyhow::Result<String> {
        let subject = ActorRef::from_str(given)
            .map_err(|e| anyhow::anyhow!("Ask: `for` is not an actor address: {e}"))?;
        anyhow::ensure!(
            matches!(subject, ActorRef::User(_) | ActorRef::Visitor(_)),
            "Ask: `for` names a person (user: or visitor:), not {given}"
        );
        let wanted = self.port.actor_info(&subject).await;
        let roster = self.port.participants(channel).await?;
        roster
            .into_iter()
            .find(|member| member.identity_key == wanted.identity_key)
            .map(|member| member.address)
            .ok_or_else(|| anyhow::anyhow!("Ask: {given} is not in this conversation"))
    }
}

fn parse_questions(params: &serde_json::Value) -> anyhow::Result<Vec<Question>> {
    let items = params
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Ask requires 'questions' (array)"))?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let n = i + 1;
            let text = item
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Ask: question {n} needs `text`"))?;
            let options = match item.get("options") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(serde_json::Value::Array(options)) => options
                    .iter()
                    .map(|o| o.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Ask: question {n}: every option must be a string")
                    })?,
                Some(_) => anyhow::bail!("Ask: question {n}: `options` must be an array"),
            };
            let multi = item
                .get("multi")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Ok(Question {
                text: text.to_string(),
                options,
                multi,
            })
        })
        .collect()
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &'static str {
        ASK_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"Ask questions you need answered before you can go on. Nothing waits: call `Ask`, then end your turn with a short message that sets the questions in context (one line is enough; not `NO_REPLY`: the card goes with your reply). Don't repeat the questions in that message.

- In a conversation, the questions go with your reply as one card the person answers together: pick from the options you give (one, or several when `multi`), or write their own words. The answer arrives as their next message. The card stays open until they answer it or you ask again.
- In a task someone delegated to you, the questions go back to the run that delegated it with your result (`needs_input`). It answers by continuing this session; you then carry on.

Use it when you cannot make a reasonable choice yourself, or the person must decide: several questions at once, a choice among a few, or a question that needs an answer. For one open question in a conversation, just ask it in your reply. Never to ask for permission: the runtime asks the owner itself, and an answer of "Approve" decides nothing.

1–{MAX_QUESTIONS} questions, each at most {MAX_QUESTION_CHARS} characters, with no options (free text) or 2–{MAX_OPTIONS} options of one line up to {MAX_OPTION_CHARS} characters. `for` (a group only) names the member who should answer. A later call replaces the earlier one; questions replace suggested replies. Not in cron or other work with no one to answer.

## Result
`{{success, to: "conversation" | "delegator", questions}}` (`channel` in a conversation)."#
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "The question." },
                            "options": {
                                "type": "array",
                                "items": { "type": "string" },
                                "maxItems": MAX_OPTIONS,
                                "description": "2–4 short choices; omit for a free-text question."
                            },
                            "multi": {
                                "type": "boolean",
                                "description": "Whether several options may be chosen."
                            }
                        },
                        "required": ["text"]
                    },
                    "description": "The questions, in the order to ask them."
                },
                "for": {
                    "type": "string",
                    "description": "In a group: the member who should answer (user:… or visitor:… address)."
                }
            },
            "required": ["questions"]
        })
    }

    fn parallelizable(&self) -> bool {
        // Replaces the run's staged questions: calls apply in order.
        false
    }

    async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::anyhow!(
            "Ask requires a ToolContext (use execute_with_context)"
        ))
    }

    async fn execute_with_context(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<serde_json::Value> {
        let questions = parse_questions(&params)?;
        let for_given = params
            .get("for")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let ask = peko_channel::validate_ask(&Ask {
            for_actor: None,
            questions,
        })
        .map_err(|e| anyhow::anyhow!("Ask: {e}"))?;
        let principal = ctx
            .principal_id
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| PrincipalId(p.to_string()))
            .ok_or_else(|| anyhow::anyhow!("Ask requires a principal"))?;

        // The conversation's own session: the questions ride its reply.
        let channel = ctx
            .acting_for()
            .compartment
            .channel()
            .and_then(ChannelId::parse);
        if let Some(channel) = channel {
            if super::suggest::is_conversation_session(
                self.port.as_ref(),
                &channel,
                &principal,
                ctx,
            )
            .await?
            {
                let for_actor = match for_given {
                    Some(given) => Some(self.resolve_for(&channel, given).await?),
                    None => None,
                };
                let ask = Ask { for_actor, ..ask };
                crate::principal::reply_options::stage_ask(
                    self.port.as_ref(),
                    &channel,
                    &principal,
                    Some(ask.clone()),
                )
                .await;
                return Ok(json!({
                    "success": true,
                    "to": "conversation",
                    "channel": channel.as_str(),
                    "questions": ask.questions,
                    "note": "The card goes out with your reply, the message you write next; \
                             text you wrote beside this call is not shown. Now reply with one \
                             short line that introduces the card, without repeating the \
                             questions (not NO_REPLY).",
                }));
            }
        }

        // A delegated run: the questions go back with its result.
        anyhow::ensure!(
            for_given.is_none(),
            "Ask: `for` names who answers in a conversation; this run's questions go to \
             the run that delegated it"
        );
        let session = ctx.session_id.as_deref().unwrap_or_default();
        anyhow::ensure!(
            crate::agents::delegated_asks::stage(session, Some(ask.clone())),
            "Ask: this run has no one to ask — it is not a conversation's reply or a \
             delegated task. Write to your owner with ChannelSend instead"
        );
        Ok(json!({
            "success": true,
            "to": "delegator",
            "questions": ask.questions,
            "note": "The questions go back with your result. Now end your turn with a short \
                     report of where you are.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::principal::reply_options;
    use crate::tools::builtin::test_harness::{Caller, ToolHarness, PRINCIPAL};
    use peko_channel::ChannelId;
    use peko_subject::PrincipalId;
    use peko_tools_core::{ActingFor, Compartment, Origin, Trust};
    use serde_json::json;

    fn in_conversation(channel: &str, trust: Trust, session: &str) -> Caller {
        let mut caller = Caller::acting_for(ActingFor {
            actor: "user:local:local".into(),
            name: None,
            trust,
            compartment: Compartment::Conversation {
                channel: channel.into(),
            },
            origin: Origin::Ingress { channel: None },
            owner_request: false,
        });
        caller.session = session.into();
        caller
    }

    async fn conversation(h: &ToolHarness, session: &str) -> ChannelId {
        let channel = h.channel(PRINCIPAL, "chat").await;
        h.channels
            .bind(&channel, &PrincipalId(PRINCIPAL.into()), session)
            .await
            .unwrap();
        channel
    }

    const SESSION: &str = "4c1d7a2e-1b6f-4e0a-9d3c-5a6b7c8d9e01";

    fn two_questions() -> serde_json::Value {
        json!({ "questions": [
            { "text": " Which day? ", "options": ["Tue", "Thu"] },
            { "text": "Which rooms?", "options": ["A", "B", "C"], "multi": true }
        ]})
    }

    /// ADR-114 D2: the conversation's own run stages questions for its
    /// reply, trimmed, and they replace staged suggested replies.
    #[tokio::test]
    async fn the_conversation_stages_questions_for_its_reply() {
        let h = ToolHarness::new().await;
        let channel = conversation(&h, SESSION).await;
        let caller = in_conversation(channel.as_str(), Trust::Untrusted, SESSION);
        h.call_as(&caller, "Suggest", json!({ "replies": ["A", "B"] }))
            .await
            .ok();
        let value = h.call_as(&caller, "Ask", two_questions()).await.ok();
        assert_eq!(value["to"], "conversation");
        assert_eq!(value["questions"][0]["text"], "Which day?");
        let staged = reply_options::take(
            h.channels.as_ref(),
            &channel,
            &PrincipalId(PRINCIPAL.into()),
        )
        .await;
        assert!(staged.options.is_empty(), "questions replace options");
        let ask = staged.ask.expect("questions staged");
        assert_eq!(ask.questions.len(), 2);
        assert!(ask.questions[1].multi);
    }

    /// ADR-114 D4: a delegated run's questions go to its delegator.
    #[tokio::test]
    async fn a_delegated_run_asks_its_delegator() {
        let h = ToolHarness::new().await;
        let worker = uuid::Uuid::new_v4().to_string();
        let mut caller = Caller::default();
        caller.session = worker.clone();

        h.call_as(&caller, "Ask", two_questions())
            .await
            .err("no one to ask");

        crate::agents::delegated_asks::expect_report(&worker);
        let value = h.call_as(&caller, "Ask", two_questions()).await.ok();
        assert_eq!(value["to"], "delegator");
        let mut with_for = two_questions();
        with_for["for"] = json!("user:local:local");
        h.call_as(&caller, "Ask", with_for)
            .await
            .err("names who answers in a conversation");
        let asked = crate::agents::delegated_asks::finish(&worker).expect("staged");
        assert_eq!(asked.questions[0].text, "Which day?");
    }

    #[tokio::test]
    async fn bad_calls_are_refused() {
        let h = ToolHarness::new().await;
        let channel = conversation(&h, SESSION).await;
        let caller = in_conversation(channel.as_str(), Trust::Owner, SESSION);
        for (params, expected) in [
            (json!({}), "'questions'"),
            (json!({ "questions": [] }), "questions"),
            (
                json!({ "questions": [{ "text": "Pick", "options": ["only"] }] }),
                "2 to 4 options",
            ),
            (
                json!({ "questions": [{ "text": "Pick", "multi": true }] }),
                "offers none",
            ),
            (
                json!({ "questions": [{ "text": "?" }], "for": "user:local:nobody" }),
                "not in this conversation",
            ),
            (
                json!({ "questions": [{ "text": "?" }], "for": "principal:local:x" }),
                "names a person",
            ),
        ] {
            h.call_as(&caller, "Ask", params).await.err(expected);
        }
    }
}
