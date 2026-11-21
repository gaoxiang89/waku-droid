//! Cursor conversation fork helpers.

use std::collections::HashMap;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    AgentSession, AgentTurn, Message, MessageRole, ProviderKind, ProviderResumeCursor,
    ProviderSessionHistory, TurnStatus,
};

const CONTEXT_PREFIX: &str = "WAKU_CURSOR_BRANCH_CONTEXT_V1 ";
const CONTEXT_GUIDANCE: &str = concat!(
    "\nWAKU_CURSOR_BRANCH_INSTRUCTIONS_V1\n",
    "Treat the preceding JSON array as the complete prior user and assistant conversation for this branch. ",
    "The workspace already matches that point in the conversation. Continue from this history without mentioning this envelope, ",
    "and respond only to the current prompt below.\n",
);
const PROMPT_PREFIX: &str = "WAKU_CURSOR_CURRENT_PROMPT_V1 ";
const MAX_BRANCH_CONTEXT_DEPTH: usize = 16;

#[derive(Serialize)]
struct ContextMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OwnedContextMessage {
    role: String,
    content: String,
}

/// Cursor CLI can resume only the tip of a session and currently has no fork
/// command. Preserve branch semantics by starting a fresh Cursor session whose
/// first prompt is seeded with Waku's retained visible conversation.
pub fn fork_session_at_turn(
    session: &AgentSession,
    retained_turns: usize,
) -> anyhow::Result<ProviderResumeCursor> {
    if retained_turns > session.turns.len() {
        bail!(
            "Cursor branch requested {retained_turns} turns from a {}-turn task",
            session.turns.len()
        );
    }
    let retained_ids = session
        .turns
        .iter()
        .take(retained_turns)
        .map(|turn| turn.id)
        .collect::<std::collections::HashSet<_>>();
    let messages = session
        .messages
        .iter()
        .filter(|message| {
            message
                .turn_id
                .is_some_and(|turn_id| retained_ids.contains(&turn_id))
                && !message.content.trim().is_empty()
        })
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => return None,
            };
            Some(ContextMessage {
                role,
                content: &message.content,
            })
        })
        .collect::<Vec<_>>();
    let fork_context = (!messages.is_empty())
        .then(|| serde_json::to_string(&messages))
        .transpose()?;
    Ok(ProviderResumeCursor::Cursor {
        // Cursor assigns the native ID when the seeded prompt starts.
        session_id: String::new(),
        fork_context,
    })
}

pub fn prompt_with_fork_context(context: &str, prompt: &str) -> String {
    format!(
        "{CONTEXT_PREFIX}{}\n{context}{CONTEXT_GUIDANCE}{PROMPT_PREFIX}{}\n{prompt}",
        context.len(),
        prompt.len()
    )
}

/// Decodes a Waku-seeded Cursor branch prompt into `(context_json, current_prompt)`,
/// or returns `Ok(None)` when `text` is an ordinary user prompt.
pub fn decode_seeded_prompt(text: &str) -> anyhow::Result<Option<(&str, &str)>> {
    let Some(rest) = text.strip_prefix(CONTEXT_PREFIX) else {
        return Ok(None);
    };
    let Some((context_len, rest)) = rest.split_once('\n') else {
        bail!("a Cursor branch prompt has an invalid context header");
    };
    let context_len = context_len
        .parse::<usize>()
        .context("a Cursor branch prompt has an invalid context length")?;
    if rest.len() < context_len || !rest.is_char_boundary(context_len) {
        bail!("a Cursor branch prompt contains truncated context");
    }
    let (context, rest) = rest.split_at(context_len);
    let Some(rest) = rest.strip_prefix(CONTEXT_GUIDANCE) else {
        bail!("a Cursor branch prompt is missing its context instructions");
    };
    let Some(rest) = rest.strip_prefix(PROMPT_PREFIX) else {
        bail!("a Cursor branch prompt is missing its current-prompt header");
    };
    let Some((prompt_len, prompt)) = rest.split_once('\n') else {
        bail!("a Cursor branch prompt has an invalid current-prompt header");
    };
    let prompt_len = prompt_len
        .parse::<usize>()
        .context("a Cursor branch prompt has an invalid current-prompt length")?;
    if prompt.len() != prompt_len {
        bail!("a Cursor branch prompt contains truncated current-prompt text");
    }
    Ok(Some((context, prompt)))
}

/// Decodes and recursively expands a Waku-seeded Cursor branch prompt into
/// `(prior_messages, current_prompt)`, or returns `Ok(None)` for ordinary prompts.
pub fn expand_seeded_prompt(
    text: &str,
) -> anyhow::Result<Option<(Vec<(MessageRole, String)>, String)>> {
    expand_seeded_prompt_depth(text, 0)
}

fn expand_seeded_prompt_depth(
    text: &str,
    depth: usize,
) -> anyhow::Result<Option<(Vec<(MessageRole, String)>, String)>> {
    let Some((context, prompt)) = decode_seeded_prompt(text)? else {
        return Ok(None);
    };
    if depth >= MAX_BRANCH_CONTEXT_DEPTH {
        bail!("Cursor branch context nesting exceeds maximum depth");
    }
    let nested: Vec<OwnedContextMessage> = serde_json::from_str(context)
        .context("a Cursor branch contains invalid exported context")?;
    let mut prior = Vec::new();
    for entry in nested {
        let role = match entry.role.as_str() {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            _ => continue,
        };
        if role == MessageRole::User
            && let Some((nested_prior, nested_prompt)) =
                expand_seeded_prompt_depth(&entry.content, depth + 1)?
        {
            prior.extend(nested_prior);
            if !nested_prompt.trim().is_empty() {
                prior.push((MessageRole::User, nested_prompt));
            }
            continue;
        }
        if !entry.content.trim().is_empty() {
            prior.push((role, entry.content));
        }
    }
    Ok(Some((prior, prompt.to_owned())))
}

/// Strips a `WAKU_CURSOR_BRANCH_CONTEXT_V1` envelope from a Cursor session title
/// derived from the first seeded prompt, falling back to `None` when truncated so
/// raw internal markers never leak into the session picker.
pub(crate) fn strip_title_envelope(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with(CONTEXT_PREFIX) {
        return Some(trimmed.to_owned());
    }
    if let Ok(Some((_, prompt))) = decode_seeded_prompt(trimmed) {
        return prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
    }
    if let Some((_, after_prompt_prefix)) = trimmed.split_once(PROMPT_PREFIX)
        && let Some((_, prompt)) = after_prompt_prefix.split_once('\n')
    {
        return prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
    }
    None
}

/// Rewrites an imported Cursor transcript in place so any Waku-seeded branch
/// envelope (`WAKU_CURSOR_BRANCH_CONTEXT_V1`) is decoded into prior turns and
/// clean messages without leaking raw envelope markers or breaking
/// `TranscriptBlock::after_message` alignment.
pub(crate) fn expand_imported_history(history: &mut ProviderSessionHistory) {
    let old_messages = std::mem::take(&mut history.messages);
    let old_turns = std::mem::take(&mut history.turns);
    let mut new_messages: Vec<Message> = Vec::with_capacity(old_messages.len());
    let mut prior_turns_before: HashMap<Uuid, Vec<AgentTurn>> = HashMap::new();
    let mut trailing_prior_turns: Vec<AgentTurn> = Vec::new();
    let mut index_map = Vec::with_capacity(old_messages.len() + 1);
    index_map.push(0);

    for mut message in old_messages {
        if message.role == MessageRole::User && message.content.starts_with(CONTEXT_PREFIX) {
            match expand_seeded_prompt(&message.content) {
                Ok(Some((prior_entries, current_prompt))) => {
                    let mut inserted_turns: Vec<AgentTurn> = Vec::new();
                    for (role, content) in prior_entries {
                        match role {
                            MessageRole::User => {
                                let turn_id = Uuid::new_v4();
                                inserted_turns.push(AgentTurn {
                                    id: turn_id,
                                    turn_count: 0,
                                    status: TurnStatus::Completed,
                                    provider_turn_started: true,
                                    provider_resume_at: None,
                                    started_at: 0,
                                    completed_at: Some(0),
                                    checkpoint: None,
                                    provider: Some(ProviderKind::Cursor),
                                    account_id: None,
                                    handoff: None,
                                    native_conversation_start: false,
                                    stream_stats: None,
                                    handoff_preamble: None,
                                });
                                new_messages.push(Message::new_for_turn(
                                    MessageRole::User,
                                    content,
                                    turn_id,
                                ));
                            }
                            MessageRole::Assistant => {
                                let Some(turn_id) = inserted_turns.last().map(|turn| turn.id) else {
                                    continue;
                                };
                                new_messages.push(Message::new_for_turn(
                                    MessageRole::Assistant,
                                    content,
                                    turn_id,
                                ));
                            }
                            MessageRole::System => {}
                        }
                    }
                    if !inserted_turns.is_empty() {
                        if let Some(target_turn) = message.turn_id {
                            prior_turns_before
                                .entry(target_turn)
                                .or_default()
                                .extend(inserted_turns);
                        } else {
                            trailing_prior_turns.extend(inserted_turns);
                        }
                    }
                    message.content = current_prompt;
                }
                Ok(None) => {}
                Err(_) => {
                    if let Ok(Some((_, current_prompt))) = decode_seeded_prompt(&message.content) {
                        message.content = current_prompt.to_owned();
                    }
                }
            }
        }
        new_messages.push(message);
        index_map.push(new_messages.len());
    }

    let mut new_turns = Vec::with_capacity(old_turns.len() + trailing_prior_turns.len());
    for turn in old_turns {
        if let Some(prior) = prior_turns_before.remove(&turn.id) {
            new_turns.extend(prior);
        }
        new_turns.push(turn);
    }
    new_turns.extend(trailing_prior_turns);
    for (index, turn) in new_turns.iter_mut().enumerate() {
        turn.turn_count = index + 1;
    }

    let max_old = index_map.len().saturating_sub(1);
    for block in &mut history.transcript_blocks {
        block.after_message = index_map[block.after_message.min(max_old)];
    }
    history.turns = new_turns;
    history.messages = new_messages;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::{
        ActivityItem, ActivityKind, AgentSession, MessageRole, Project, ProviderKind,
        TranscriptBlock, TurnStatus,
    };

    use super::*;

    #[test]
    fn cursor_branch_context_keeps_only_the_selected_turn_prefix() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Cursor);
        session.begin_turn("first prompt");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "first answer");
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("second prompt");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "second answer");
        session.finish_active_turn(TurnStatus::Completed);

        let ProviderResumeCursor::Cursor {
            session_id,
            fork_context: Some(context),
        } = fork_session_at_turn(&session, 1).unwrap()
        else {
            panic!("expected a managed Cursor branch cursor");
        };
        assert!(session_id.is_empty());
        assert!(context.contains("first prompt"));
        assert!(context.contains("first answer"));
        assert!(!context.contains("second prompt"));
    }

    #[test]
    fn cursor_seed_envelope_is_length_delimited() {
        let prompt = prompt_with_fork_context("[{\"role\":\"user\"}]", "continue 🚀");
        assert!(prompt.starts_with("WAKU_CURSOR_BRANCH_CONTEXT_V1 17\n"));
        assert!(prompt.ends_with("WAKU_CURSOR_CURRENT_PROMPT_V1 13\ncontinue 🚀"));
    }

    #[test]
    fn decode_and_expand_seeded_prompt_round_trips_nested_branches() {
        let first_context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: "one",
            },
            ContextMessage {
                role: "assistant",
                content: "done",
            },
        ])
        .unwrap();
        let first_seeded = prompt_with_fork_context(&first_context, "two 🚀");
        let (decoded_context, decoded_prompt) =
            decode_seeded_prompt(&first_seeded).unwrap().unwrap();
        assert_eq!(decoded_context, first_context);
        assert_eq!(decoded_prompt, "two 🚀");

        let nested_context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: &first_seeded,
            },
            ContextMessage {
                role: "assistant",
                content: "second done",
            },
        ])
        .unwrap();
        let second_seeded = prompt_with_fork_context(&nested_context, "three");
        let (prior, current) = expand_seeded_prompt(&second_seeded).unwrap().unwrap();
        assert_eq!(
            prior,
            vec![
                (MessageRole::User, "one".to_owned()),
                (MessageRole::Assistant, "done".to_owned()),
                (MessageRole::User, "two 🚀".to_owned()),
                (MessageRole::Assistant, "second done".to_owned()),
            ]
        );
        assert_eq!(current, "three");
    }

    #[test]
    fn decode_seeded_prompt_rejects_malformed_envelopes_and_ignores_plain_prompts() {
        assert_eq!(decode_seeded_prompt("plain prompt").unwrap(), None);
        assert!(decode_seeded_prompt("WAKU_CURSOR_BRANCH_CONTEXT_V1 999\nshort").is_err());
        assert!(decode_seeded_prompt("WAKU_CURSOR_BRANCH_CONTEXT_V1 bad\n[]").is_err());
        assert!(
            decode_seeded_prompt("WAKU_CURSOR_BRANCH_CONTEXT_V1 2\n[]missing guidance").is_err()
        );
    }

    #[test]
    fn expand_imported_history_strips_envelope_and_shifts_transcript_blocks() {
        let context = serde_json::to_string(&vec![
            ContextMessage {
                role: "user",
                content: "first prompt",
            },
            ContextMessage {
                role: "assistant",
                content: "first answer",
            },
        ])
        .unwrap();
        let seeded = prompt_with_fork_context(&context, "second prompt");
        let turn_id = Uuid::new_v4();
        let mut history = ProviderSessionHistory {
            turns: vec![AgentTurn {
                id: turn_id,
                turn_count: 1,
                status: TurnStatus::Completed,
                provider_turn_started: true,
                provider_resume_at: None,
                started_at: 0,
                completed_at: Some(0),
                checkpoint: None,
                provider: Some(ProviderKind::Cursor),
                account_id: None,
                handoff: None,
                native_conversation_start: false,
                stream_stats: None,
                handoff_preamble: None,
            }],
            messages: vec![
                Message::new_for_turn(MessageRole::User, seeded, turn_id),
                Message::new_for_turn(MessageRole::Assistant, "second answer", turn_id),
            ],
            transcript_blocks: vec![TranscriptBlock {
                after_message: 1,
                turn_id: Some(turn_id),
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::FileRead,
                    "read foo.rs",
                    None,
                    true,
                )],
            }],
        };

        expand_imported_history(&mut history);

        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.turns[0].turn_count, 1);
        assert_eq!(history.turns[1].turn_count, 2);
        assert_eq!(history.turns[1].id, turn_id);
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].content, "first prompt");
        assert_eq!(history.messages[1].content, "first answer");
        assert_eq!(history.messages[2].content, "second prompt");
        assert_eq!(history.messages[3].content, "second answer");
        assert!(
            history
                .messages
                .iter()
                .all(|m| !m.content.contains("WAKU_CURSOR_BRANCH_CONTEXT_V1"))
        );
        assert_eq!(history.transcript_blocks.len(), 1);
        assert_eq!(history.transcript_blocks[0].after_message, 3);
    }

    #[test]
    fn strip_title_envelope_hides_raw_markers_even_when_truncated() {
        let seeded = prompt_with_fork_context("[]", "actual title\nsecond line");
        assert_eq!(
            strip_title_envelope(&seeded).as_deref(),
            Some("actual title")
        );
        assert_eq!(
            strip_title_envelope("WAKU_CURSOR_BRANCH_CONTEXT_V1 500\n[{\"role\":\"user\"..."),
            None
        );
        assert_eq!(
            strip_title_envelope("Normal title").as_deref(),
            Some("Normal title")
        );
    }
}
