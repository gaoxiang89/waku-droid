use anyhow::{Context as _, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::WireDriverEvent;
use crate::computer_use::{ComputerTarget, ComputerUsePhase, ComputerUseState};
use crate::model::{
    ActivityKind, DriverEvent, PermissionOption, SubmittedHandoff, UserInputQuestion,
};

pub fn decode_enum<T: DeserializeOwned>(value: &str) -> anyhow::Result<T> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid protocol enum value {value:?}"))
}

pub fn encode_enum<T: Serialize>(value: T) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("protocol enum did not serialize as a string"))
}

pub fn event_to_wire(event: DriverEvent) -> anyhow::Result<WireDriverEvent> {
    let (kind, payload) = match event {
        DriverEvent::RuntimeEventCursorAdvanced(_) => {
            bail!("client-only runtime cursors cannot be sent by the daemon")
        }
        DriverEvent::Connected { provider_cursor } => {
            ("connected", serde_json::to_value(provider_cursor)?)
        }
        DriverEvent::AgentPresetSelected(preset) => {
            ("agentPresetSelected", serde_json::to_value(preset)?)
        }
        DriverEvent::AutoTitleUpdated(title) => ("autoTitleUpdated", serde_json::to_value(title)?),
        DriverEvent::AvailableCommands(commands) => {
            ("availableCommands", serde_json::to_value(commands)?)
        }
        DriverEvent::TurnStarted => ("turnStarted", Value::Null),
        DriverEvent::TurnParked => ("turnParked", Value::Null),
        DriverEvent::TextDelta(text) => ("textDelta", Value::String(text)),
        DriverEvent::ReasoningDelta(text) => ("reasoningDelta", Value::String(text)),
        DriverEvent::Activity {
            id,
            kind,
            title,
            detail,
            complete,
        } => (
            "activity",
            json!({
                "id": id,
                "kind": kind,
                "title": title,
                "detail": detail,
                "complete": complete,
            }),
        ),
        DriverEvent::RichActivity(activity) => ("richActivity", serde_json::to_value(activity)?),
        DriverEvent::BackgroundWork(work) => ("backgroundWork", serde_json::to_value(work)?),
        DriverEvent::Permission {
            request_id,
            title,
            detail,
            options,
        } => (
            "permission",
            json!({
                "requestId": request_id,
                "title": title,
                "detail": detail,
                "options": options,
            }),
        ),
        DriverEvent::UserInputRequested {
            request_id,
            questions,
        } => (
            "userInputRequested",
            json!({
                "requestId": request_id,
                "questions": questions,
            }),
        ),
        DriverEvent::ComputerUseUpdated(state) => (
            "computerUseUpdated",
            serde_json::to_value(ComputerUseWire {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })?,
        ),
        DriverEvent::PromptSubmitted {
            message,
            turn_id,
            message_id,
            handoff,
        } => {
            let mut payload =
                json!({ "message": message, "turnId": turn_id, "messageId": message_id });
            if let Some(handoff) = handoff {
                payload["handoff"] = serde_json::to_value(handoff)?;
            }
            ("promptSubmitted", payload)
        }
        DriverEvent::SteerAccepted { message } => ("steerAccepted", json!({ "message": message })),
        DriverEvent::SteerRejected { message, reason } => (
            "steerRejected",
            json!({ "message": message, "reason": reason }),
        ),
        DriverEvent::UsageUpdated {
            context_tokens,
            context_window,
        } => (
            "usageUpdated",
            json!({
                "contextTokens": context_tokens,
                "contextWindow": context_window,
            }),
        ),
        DriverEvent::PlanUsageUpdated(usage) => ("planUsageUpdated", serde_json::to_value(usage)?),
        DriverEvent::GoalUpdated(goal) => ("goalUpdated", serde_json::to_value(goal)?),
        DriverEvent::TurnFinished { success, summary } => (
            "turnFinished",
            json!({ "success": success, "summary": summary }),
        ),
        DriverEvent::Error(error) => ("error", Value::String(error)),
        DriverEvent::ProcessExited => ("processExited", Value::Null),
    };
    Ok(WireDriverEvent::new(kind, payload))
}

pub fn event_from_wire(event: WireDriverEvent) -> anyhow::Result<DriverEvent> {
    let payload = event.payload;
    Ok(match event.kind.as_str() {
        "connected" => DriverEvent::Connected {
            provider_cursor: serde_json::from_value(payload)?,
        },
        "agentPresetSelected" => DriverEvent::AgentPresetSelected(serde_json::from_value(payload)?),
        "autoTitleUpdated" => DriverEvent::AutoTitleUpdated(serde_json::from_value(payload)?),
        "availableCommands" => DriverEvent::AvailableCommands(serde_json::from_value(payload)?),
        "turnStarted" => DriverEvent::TurnStarted,
        "turnParked" => DriverEvent::TurnParked,
        "textDelta" => DriverEvent::TextDelta(serde_json::from_value(payload)?),
        "reasoningDelta" => DriverEvent::ReasoningDelta(serde_json::from_value(payload)?),
        "activity" => {
            let activity: ActivityWire = serde_json::from_value(payload)?;
            DriverEvent::Activity {
                id: activity.id,
                kind: activity.kind,
                title: activity.title,
                detail: activity.detail,
                complete: activity.complete,
            }
        }
        "richActivity" => DriverEvent::RichActivity(serde_json::from_value(payload)?),
        "backgroundWork" => DriverEvent::BackgroundWork(serde_json::from_value(payload)?),
        "permission" => {
            let permission: PermissionWire = serde_json::from_value(payload)?;
            DriverEvent::Permission {
                request_id: permission.request_id,
                title: permission.title,
                detail: permission.detail,
                options: permission.options,
            }
        }
        "userInputRequested" => {
            let request: UserInputWire = serde_json::from_value(payload)?;
            DriverEvent::UserInputRequested {
                request_id: request.request_id,
                questions: request.questions,
            }
        }
        "computerUseUpdated" => {
            let state: ComputerUseWire = serde_json::from_value(payload)?;
            DriverEvent::ComputerUseUpdated(ComputerUseState {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })
        }
        "promptSubmitted" => {
            let submitted: SubmittedPromptWire = serde_json::from_value(payload)?;
            DriverEvent::PromptSubmitted {
                message: submitted.message,
                turn_id: submitted.turn_id,
                message_id: submitted.message_id,
                handoff: submitted.handoff,
            }
        }
        "steerAccepted" => {
            let steer: AcceptedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerAccepted {
                message: steer.message,
            }
        }
        "steerRejected" => {
            let steer: RejectedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerRejected {
                message: steer.message,
                reason: steer.reason,
            }
        }
        "usageUpdated" => {
            let usage: UsageWire = serde_json::from_value(payload)?;
            DriverEvent::UsageUpdated {
                context_tokens: usage.context_tokens,
                context_window: usage.context_window,
            }
        }
        "planUsageUpdated" => DriverEvent::PlanUsageUpdated(serde_json::from_value(payload)?),
        "goalUpdated" => DriverEvent::GoalUpdated(serde_json::from_value(payload)?),
        "turnFinished" => {
            let finished: TurnFinishedWire = serde_json::from_value(payload)?;
            DriverEvent::TurnFinished {
                success: finished.success,
                summary: finished.summary,
            }
        }
        "error" => DriverEvent::Error(serde_json::from_value(payload)?),
        "processExited" => DriverEvent::ProcessExited,
        kind => bail!("daemon sent an unsupported driver event {kind:?}"),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmittedPromptWire {
    message: String,
    turn_id: Uuid,
    message_id: Uuid,
    #[serde(default)]
    handoff: Option<SubmittedHandoff>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityWire {
    id: Option<String>,
    kind: ActivityKind,
    title: String,
    detail: Option<String>,
    complete: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionWire {
    request_id: String,
    title: String,
    detail: String,
    options: Vec<PermissionOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInputWire {
    request_id: String,
    questions: Vec<UserInputQuestion>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComputerUseWire {
    target: Option<ComputerTarget>,
    phase: ComputerUsePhase,
    visible: bool,
    image_url: Option<String>,
}

#[derive(Deserialize)]
struct AcceptedSteerWire {
    message: String,
}

#[derive(Deserialize)]
struct RejectedSteerWire {
    message: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWire {
    context_tokens: Option<u64>,
    context_window: Option<u64>,
}

#[derive(Deserialize)]
struct TurnFinishedWire {
    success: bool,
    summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ActivityFileChange, ActivityFileChangeStatus, ActivityItem, BackgroundWorkEvent,
        BackgroundWorkItem, BackgroundWorkKey, BackgroundWorkKind, BackgroundWorkStatus,
        ProviderKind, ProviderResumeCursor, ReasoningBlock, ReportedCommand, RuntimeEventCursor,
        RuntimeMode, ThreadGoal, ThreadGoalStatus, TurnHandoff, UserInputOption, UserInputQuestion,
    };
    use crate::usage::{PlanUsage, PlanWindow};

    #[test]
    fn encode_and_decode_enum_round_trip_and_reject_unknown_values() {
        let provider = encode_enum(ProviderKind::Claude).unwrap();
        assert_eq!(provider, "claude");
        assert_eq!(
            decode_enum::<ProviderKind>(&provider).unwrap(),
            ProviderKind::Claude
        );

        let mode = encode_enum(RuntimeMode::AutoAcceptEdits).unwrap();
        assert_eq!(mode, "autoAcceptEdits");
        assert_eq!(
            decode_enum::<RuntimeMode>(&mode).unwrap(),
            RuntimeMode::AutoAcceptEdits
        );

        assert!(decode_enum::<ProviderKind>("unknownProvider").is_err());
        assert!(encode_enum(json!({ "not": "a string" })).is_err());
    }

    #[test]
    fn goal_updates_round_trip_through_the_daemon_wire() {
        let wire = event_to_wire(DriverEvent::GoalUpdated(Some(ThreadGoal {
            objective: "Ship the feature".into(),
            status: ThreadGoalStatus::UsageLimited,
            token_budget: Some(50_000),
            tokens_used: 12_500,
            time_used_seconds: 90,
        })))
        .unwrap();
        assert_eq!(wire.kind, "goalUpdated");
        // The status spelling is Codex's own camelCase vocabulary.
        assert_eq!(wire.payload["status"], "usageLimited");

        let DriverEvent::GoalUpdated(Some(goal)) = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(goal.objective, "Ship the feature");
        assert_eq!(goal.status, ThreadGoalStatus::UsageLimited);
        assert_eq!(goal.token_budget, Some(50_000));

        let cleared = event_to_wire(DriverEvent::GoalUpdated(None)).unwrap();
        assert!(matches!(
            event_from_wire(cleared).unwrap(),
            DriverEvent::GoalUpdated(None)
        ));
    }

    #[test]
    fn structured_user_input_round_trips_through_the_daemon_wire() {
        let wire = event_to_wire(DriverEvent::UserInputRequested {
            request_id: "request-1".into(),
            questions: vec![UserInputQuestion {
                id: "deployment".into(),
                header: "Environment".into(),
                question: "Where should this deploy?".into(),
                options: vec![UserInputOption {
                    label: "Preview".into(),
                    description: Some("Create a preview deployment".into()),
                }],
                multi_select: false,
                required: true,
                accepts_custom: true,
            }],
        })
        .unwrap();
        assert_eq!(wire.kind, "userInputRequested");

        let DriverEvent::UserInputRequested {
            request_id,
            questions,
        } = event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(request_id, "request-1");
        assert_eq!(questions[0].id, "deployment");
        assert_eq!(questions[0].options[0].label, "Preview");
    }

    #[test]
    fn all_driver_event_variants_round_trip_through_the_daemon_wire() {
        // Client-only cursor updates must never cross the daemon wire.
        let cursor_event = DriverEvent::RuntimeEventCursorAdvanced(RuntimeEventCursor {
            runtime_id: Uuid::from_u128(1),
            epoch: Uuid::from_u128(2),
            sequence: 42,
        });
        assert!(event_to_wire(cursor_event).is_err());

        // Unsupported wire kinds must fail decoding cleanly.
        assert!(
            event_from_wire(WireDriverEvent::new("unsupportedEventKind", Value::Null)).is_err()
        );

        let turn_id = Uuid::from_u128(10);
        let message_id = Uuid::from_u128(11);
        let handoff = SubmittedHandoff {
            notice_id: Uuid::from_u128(12),
            notice: "Continuing with Claude".into(),
            record: TurnHandoff {
                from_providers: vec![ProviderKind::Codex],
                covered_turns: 3,
                omitted_turns: 1,
                resumed: true,
            },
        };

        let mut rich_activity = ActivityItem::new(
            Some("call-1".into()),
            ActivityKind::FileChange,
            "Edit lib.rs",
            Some("Updated wire codec".into()),
            true,
        )
        .with_tool_name(Some("mcp__fs__edit_file"));
        rich_activity.arguments = Some(r#"{"path":"src/lib.rs"}"#.into());
        rich_activity.output = Some("ok".into());
        rich_activity.image_urls = vec!["data:image/png;base64,AAAA".into()];
        rich_activity.file_changes = vec![ActivityFileChange {
            path: "src/lib.rs".into(),
            additions: Some(4),
            deletions: Some(2),
            status: Some(ActivityFileChangeStatus::Modified),
            diff: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
        }];
        rich_activity.reasoning = Some(ReasoningBlock {
            content: "Checking invariants".into(),
            started_at_ms: 1_000,
            finished_at_ms: 1_500,
        });

        let work_key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "proc-7");
        let mut work_item = BackgroundWorkItem::new(
            BackgroundWorkKind::Process,
            "proc-7",
            "cargo test",
            BackgroundWorkStatus::Running,
        );
        work_item.command = Some("cargo test -p waku-protocol".into());
        work_item.cwd = Some("/tmp/waku".into());
        work_item.background = true;
        work_item.can_stop = true;
        work_item.control_id = Some("ctrl-7".into());

        let cases: Vec<(DriverEvent, &'static str)> = vec![
            (
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Claude {
                        session_id: "sess-1".into(),
                        resume_at: Some("msg-9".into()),
                    }),
                },
                "connected",
            ),
            (
                DriverEvent::Connected {
                    provider_cursor: None,
                },
                "connected",
            ),
            (
                DriverEvent::AgentPresetSelected(Some("code".into())),
                "agentPresetSelected",
            ),
            (
                DriverEvent::AgentPresetSelected(None),
                "agentPresetSelected",
            ),
            (
                DriverEvent::AutoTitleUpdated(Some("Deduplicate codecs".into())),
                "autoTitleUpdated",
            ),
            (DriverEvent::AutoTitleUpdated(None), "autoTitleUpdated"),
            (
                DriverEvent::AvailableCommands(vec![ReportedCommand {
                    name: "review".into(),
                    description: "Review staged changes".into(),
                    argument_hint: Some("[branch]".into()),
                }]),
                "availableCommands",
            ),
            (
                DriverEvent::PromptSubmitted {
                    message: "ship it".into(),
                    turn_id,
                    message_id,
                    handoff: None,
                },
                "promptSubmitted",
            ),
            (
                DriverEvent::PromptSubmitted {
                    message: "continue".into(),
                    turn_id,
                    message_id,
                    handoff: Some(handoff.clone()),
                },
                "promptSubmitted",
            ),
            (DriverEvent::TurnStarted, "turnStarted"),
            (DriverEvent::TurnParked, "turnParked"),
            (DriverEvent::TextDelta("hello ".into()), "textDelta"),
            (
                DriverEvent::ReasoningDelta("thinking...".into()),
                "reasoningDelta",
            ),
            (
                DriverEvent::Activity {
                    id: Some("act-1".into()),
                    kind: ActivityKind::Command,
                    title: "cargo check".into(),
                    detail: Some("Finished".into()),
                    complete: true,
                },
                "activity",
            ),
            (
                DriverEvent::RichActivity(rich_activity.clone()),
                "richActivity",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(work_item.clone())),
                "backgroundWork",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::OutputDelta {
                    key: work_key.clone(),
                    delta: "running 12 tests\n".into(),
                }),
                "backgroundWork",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::ReconcileProcesses {
                    items: vec![work_item.clone()],
                }),
                "backgroundWork",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::ReconcileLive {
                    items: vec![work_item.clone()],
                }),
                "backgroundWork",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::StopRequested(work_key.clone())),
                "backgroundWork",
            ),
            (
                DriverEvent::BackgroundWork(BackgroundWorkEvent::StopFailed {
                    key: work_key.clone(),
                    message: "already exited".into(),
                }),
                "backgroundWork",
            ),
            (
                DriverEvent::Permission {
                    request_id: "perm-1".into(),
                    title: "Run shell command".into(),
                    detail: "git status".into(),
                    options: vec![
                        PermissionOption {
                            id: "allow".into(),
                            label: "Allow once".into(),
                            allow: true,
                        },
                        PermissionOption {
                            id: "deny".into(),
                            label: "Deny".into(),
                            allow: false,
                        },
                    ],
                },
                "permission",
            ),
            (
                DriverEvent::UserInputRequested {
                    request_id: "input-1".into(),
                    questions: vec![UserInputQuestion {
                        id: "q1".into(),
                        header: "Choice".into(),
                        question: "Proceed?".into(),
                        options: vec![UserInputOption {
                            label: "Yes".into(),
                            description: None,
                        }],
                        multi_select: true,
                        required: false,
                        accepts_custom: false,
                    }],
                },
                "userInputRequested",
            ),
            (
                DriverEvent::ComputerUseUpdated(ComputerUseState {
                    target: Some(ComputerTarget {
                        window_id: 99,
                        bundle_id: "com.apple.Safari".into(),
                        team_id: Some("APPLE".into()),
                        app_name: "Safari".into(),
                        window_title: "Docs".into(),
                        width: 1440,
                        height: 900,
                    }),
                    phase: ComputerUsePhase::Running,
                    visible: true,
                    image_url: Some("data:image/png;base64,BBBB".into()),
                }),
                "computerUseUpdated",
            ),
            (
                DriverEvent::SteerAccepted {
                    message: "focus on tests".into(),
                },
                "steerAccepted",
            ),
            (
                DriverEvent::SteerRejected {
                    message: "too late".into(),
                    reason: "turn already settled".into(),
                },
                "steerRejected",
            ),
            (
                DriverEvent::UsageUpdated {
                    context_tokens: Some(42_000),
                    context_window: Some(200_000),
                },
                "usageUpdated",
            ),
            (
                DriverEvent::UsageUpdated {
                    context_tokens: None,
                    context_window: Some(1_000_000),
                },
                "usageUpdated",
            ),
            (
                DriverEvent::PlanUsageUpdated(PlanUsage {
                    plan_label: Some("Max".into()),
                    windows: vec![PlanWindow {
                        label: "5-hour".into(),
                        percent: 37.5,
                        resets_at: Some(1_800_000_000),
                    }],
                }),
                "planUsageUpdated",
            ),
            (
                DriverEvent::GoalUpdated(Some(ThreadGoal {
                    objective: "Finish batch 3".into(),
                    status: ThreadGoalStatus::Active,
                    token_budget: None,
                    tokens_used: 100,
                    time_used_seconds: 12,
                })),
                "goalUpdated",
            ),
            (
                DriverEvent::TurnFinished {
                    success: true,
                    summary: Some("All checks green".into()),
                },
                "turnFinished",
            ),
            (
                DriverEvent::TurnFinished {
                    success: false,
                    summary: None,
                },
                "turnFinished",
            ),
            (DriverEvent::Error("provider disconnected".into()), "error"),
            (DriverEvent::ProcessExited, "processExited"),
        ];

        for (event, expected_kind) in cases {
            let wire = event_to_wire(event.clone()).unwrap();
            assert_eq!(wire.kind, expected_kind);
            // Also verify the wire event survives a full JSON string round trip.
            let wire: WireDriverEvent =
                serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
            let decoded = event_from_wire(wire).unwrap();

            // Exhaustive match ensures every DriverEvent variant is checked.
            match (event, decoded) {
                (DriverEvent::RuntimeEventCursorAdvanced(_), _) => unreachable!(),
                (
                    DriverEvent::Connected {
                        provider_cursor: expected,
                    },
                    DriverEvent::Connected {
                        provider_cursor: actual,
                    },
                ) => assert_eq!(actual, expected),
                (
                    DriverEvent::AgentPresetSelected(expected),
                    DriverEvent::AgentPresetSelected(actual),
                ) => assert_eq!(actual, expected),
                (
                    DriverEvent::AutoTitleUpdated(expected),
                    DriverEvent::AutoTitleUpdated(actual),
                ) => assert_eq!(actual, expected),
                (
                    DriverEvent::AvailableCommands(expected),
                    DriverEvent::AvailableCommands(actual),
                ) => assert_eq!(actual, expected),
                (
                    DriverEvent::PromptSubmitted {
                        message: em,
                        turn_id: et,
                        message_id: emi,
                        handoff: eh,
                    },
                    DriverEvent::PromptSubmitted {
                        message: am,
                        turn_id: at,
                        message_id: ami,
                        handoff: ah,
                    },
                ) => {
                    assert_eq!((am, at, ami, ah), (em, et, emi, eh));
                }
                (DriverEvent::TurnStarted, DriverEvent::TurnStarted)
                | (DriverEvent::TurnParked, DriverEvent::TurnParked)
                | (DriverEvent::ProcessExited, DriverEvent::ProcessExited) => {}
                (DriverEvent::TextDelta(expected), DriverEvent::TextDelta(actual))
                | (DriverEvent::ReasoningDelta(expected), DriverEvent::ReasoningDelta(actual))
                | (DriverEvent::Error(expected), DriverEvent::Error(actual)) => {
                    assert_eq!(actual, expected);
                }
                (
                    DriverEvent::Activity {
                        id: eid,
                        kind: ek,
                        title: et,
                        detail: ed,
                        complete: ec,
                    },
                    DriverEvent::Activity {
                        id: aid,
                        kind: ak,
                        title: at,
                        detail: ad,
                        complete: ac,
                    },
                ) => assert_eq!((aid, ak, at, ad, ac), (eid, ek, et, ed, ec)),
                (DriverEvent::RichActivity(expected), DriverEvent::RichActivity(actual)) => {
                    assert_eq!(actual.id, expected.id);
                    assert_eq!(actual.source_id, expected.source_id);
                    assert_eq!(actual.kind, expected.kind);
                    assert_eq!(actual.title, expected.title);
                    assert_eq!(actual.tool_name, expected.tool_name);
                    assert_eq!(actual.mcp_server, expected.mcp_server);
                    assert_eq!(actual.detail, expected.detail);
                    assert_eq!(actual.arguments, expected.arguments);
                    assert_eq!(actual.output, expected.output);
                    assert_eq!(actual.image_urls, expected.image_urls);
                    assert_eq!(actual.complete, expected.complete);
                    assert_eq!(actual.file_changes.len(), expected.file_changes.len());
                    assert_eq!(actual.file_changes[0].path, expected.file_changes[0].path);
                    assert_eq!(
                        actual.reasoning.as_ref().map(|r| &r.content),
                        expected.reasoning.as_ref().map(|r| &r.content)
                    );
                }
                (DriverEvent::BackgroundWork(expected), DriverEvent::BackgroundWork(actual)) => {
                    assert_eq!(
                        serde_json::to_value(&actual).unwrap(),
                        serde_json::to_value(&expected).unwrap()
                    );
                }
                (
                    DriverEvent::Permission {
                        request_id: er,
                        title: et,
                        detail: ed,
                        options: eo,
                    },
                    DriverEvent::Permission {
                        request_id: ar,
                        title: at,
                        detail: ad,
                        options: ao,
                    },
                ) => {
                    assert_eq!((ar, at, ad), (er, et, ed));
                    assert_eq!(ao.len(), eo.len());
                    assert_eq!(ao[0].id, eo[0].id);
                    assert_eq!(ao[0].allow, eo[0].allow);
                    assert_eq!(ao[1].allow, eo[1].allow);
                }
                (
                    DriverEvent::UserInputRequested {
                        request_id: er,
                        questions: eq,
                    },
                    DriverEvent::UserInputRequested {
                        request_id: ar,
                        questions: aq,
                    },
                ) => assert_eq!((ar, aq), (er, eq)),
                (
                    DriverEvent::ComputerUseUpdated(expected),
                    DriverEvent::ComputerUseUpdated(actual),
                ) => {
                    assert_eq!(actual.target, expected.target);
                    assert_eq!(actual.phase, expected.phase);
                    assert_eq!(actual.visible, expected.visible);
                    assert_eq!(actual.image_url, expected.image_url);
                }
                (
                    DriverEvent::SteerAccepted { message: expected },
                    DriverEvent::SteerAccepted { message: actual },
                ) => assert_eq!(actual, expected),
                (
                    DriverEvent::SteerRejected {
                        message: em,
                        reason: er,
                    },
                    DriverEvent::SteerRejected {
                        message: am,
                        reason: ar,
                    },
                ) => assert_eq!((am, ar), (em, er)),
                (
                    DriverEvent::UsageUpdated {
                        context_tokens: et,
                        context_window: ew,
                    },
                    DriverEvent::UsageUpdated {
                        context_tokens: at,
                        context_window: aw,
                    },
                ) => assert_eq!((at, aw), (et, ew)),
                (
                    DriverEvent::PlanUsageUpdated(expected),
                    DriverEvent::PlanUsageUpdated(actual),
                ) => assert_eq!(actual, expected),
                (DriverEvent::GoalUpdated(expected), DriverEvent::GoalUpdated(actual)) => {
                    assert_eq!(actual, expected)
                }
                (
                    DriverEvent::TurnFinished {
                        success: es,
                        summary: em,
                    },
                    DriverEvent::TurnFinished {
                        success: as_,
                        summary: am,
                    },
                ) => assert_eq!((as_, am), (es, em)),
                (expected, actual) => {
                    panic!("driver event changed variant: expected {expected:?}, got {actual:?}");
                }
            }
        }
    }
}
