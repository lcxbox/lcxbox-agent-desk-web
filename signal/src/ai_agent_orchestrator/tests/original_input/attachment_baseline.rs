//! Opt-in real-model baseline with an isolated SQLite host and a generated attachment.
use super::*;
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
    Sensitivity,
};
use desk_diagnose_core::{
    chat::ChatMessage, conversation_attachment::delivery::externalize,
    model_egress::ModelEgressPolicy, seam::ToolOutputFormat,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, IntoActiveModel, QueryFilter};
use sha2::{Digest, Sha256};

#[actix_web::test]
#[ignore = "uses the configured live gateway and a freshly generated baseline fixture"]
async fn isolated_goal_attachment_baseline() {
    desk_diagnose_core::future::boxed(measure).await;
}

async fn measure() {
    let seed = tokio::fs::read_to_string(std::env::var("LAD_ATTACHMENT_BASELINE_SEED").unwrap())
        .await
        .unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(seed.as_bytes())),
        "5b251ac8fc901c0a43ca4dcc27566c6c612d97329092d535cf1576d984075b8f"
    );
    let directory = tempfile::tempdir().unwrap();
    let db = crate::config::test_support::Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("signal.db").display()
    ))
    .await
    .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_agent_gate::enable_test_host();
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        reasoning_contract: desk_diagnose_core::model_profile::ReasoningContract::DeepseekChat,
        model: Some(std::env::var("LAD_BASELINE_MODEL").unwrap()),
        base_url: Some(std::env::var("LAD_BASELINE_RELAY").unwrap()),
        api_key: Some(std::env::var("LAD_BASELINE_RELAY_TOKEN").unwrap()),
        max_context_bytes: Some(
            std::env::var("LAD_BASELINE_WINDOW")
                .unwrap()
                .parse()
                .unwrap(),
        ),
        runtime_max_output_tokens: 128_000,
        max_steps_per_turn: 40,
        max_same_tool_calls_per_turn: 20,
        supports_image_input: true,
        response_format: crate::model_provider::ResponseFormatMode::JsonObject,
        connection_revision: 2,
        profile_revision: 2,
        ..Default::default()
    };
    crate::model_provider::save(&db, config).await.unwrap();
    let destination = crate::model_provider::load(&db)
        .await
        .unwrap()
        .destination_identity()
        .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let _peer = register_request_device(
        &connections,
        "fixture-device",
        "Fixture device",
        desk_signal_facade::model::os::OperationSystemEnum::Mac,
    )
    .await;
    crate::device_code_store::get_or_create(&db, "fixture-device")
        .await
        .unwrap();
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let owner = actor.to_string();
    let client = "isolated-attachment";
    let run = derive_agent_conversation_key(&owner, AgentConversationScope::Personal, client);
    let ask = |id: &str, question: &str, goal| AiAgentAsk {
        start_goal: goal,
        target_refs: vec!["fixture-device".into()],
        question: question.into(),
        client_message_id: id.into(),
        conversation_id: Some(client.into()),
        client_timezone: "UTC".into(),
        ..Default::default()
    };
    let send = |input| {
        run_turn(
            connections.clone(),
            db.clone(),
            uuid::Uuid::new_v4().to_string(),
            "fixture-browser".into(),
            String::new(),
            actor,
            String::new(),
            input,
        )
    };
    send(ask(
        "seed-input",
        "Set up the synthetic attachment test. Answer READY.",
        false,
    ))
    .await;
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&run))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut session =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let policy = ModelEgressPolicy {
        destination,
        selected_source_tools: ["generate_fixture".to_owned()].into(),
        export_authorization_id: "isolated-synthetic-source".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut result = ChatMessage::tool_result("fixture-result", "synthetic-producer", &seed);
    result.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "fixture-text".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "fixture-text".into(),
            sha256: format!("{:x}", Sha256::digest(seed.as_bytes())),
            size_bytes: seed.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "fixture".into(),
            source_tool_name: "generate_fixture".into(),
            source_object_id: Some("synthetic-producer".into()),
            source_envelope_ids: vec![],
        },
        sensitivity: Sensitivity::UserContent,
        allowed_destinations: vec![policy.destination.clone()],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    });
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let result = externalize(
        &store,
        &session,
        result,
        ToolOutputFormat::Text,
        Some(&policy),
        Ok(policy.now_unix_ms),
    )
    .await
    .unwrap();
    assert_eq!(result.raw_result.as_ref().unwrap().slots.len(), 1);
    // The fixture is a host-produced attachment. Register its receipt just
    // as a delivered result, without inventing a previous model response.
    session
        .ui_references
        .observe(std::slice::from_ref(&result))
        .unwrap();
    let mut row = row.into_active_model();
    row.state_json = sea_orm::Set(session.encode_json_for_storage().unwrap());
    row.update(&db).await.unwrap();
    let question = "Analyze only the existing synthetic conversation attachment att1. It contains six consecutive groups, each 48 physical lines and 96 records (two records separated by | per line). In each goal segment read exactly its next 48 lines using read_conversation_attachment in read mode (max_bytes 16384); count result=retry records yourself. Do not search, use a device, or create children. After each of the first five groups call control_goal continue with segment and cumulative counts. After group six call control_goal complete with a JSON message containing a counts array and a total integer. If a read fails report the exact blocker.";
    send(ask("baseline-goal", question, true)).await;
    let gate = crate::ai_agent_gate::global_ai_agent_gate();
    let retry_once = std::env::var_os("LAD_BASELINE_RETRY_STALLED").is_some();
    let expect_context_limit = std::env::var_os("LAD_BASELINE_EXPECT_CONTEXT_LIMIT").is_some();
    assert!(!expect_context_limit || retry_once);
    let mut owner_retried = false;
    for _ in 0..20 {
        let goal = crate::agent_goal_store::load_latest_for_subject(&db, &run, &owner, "")
            .await
            .unwrap()
            .unwrap();
        let row = crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq(&run))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let s = desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
            .unwrap();
        let pages = s
            .conversation
            .iter()
            .filter_map(|m| m.attachment_read.as_deref())
            .collect::<Vec<_>>();
        let windows = pages
            .iter()
            .map(|p| {
                (
                    p.input.start_line,
                    p.input.end_line,
                    p.body_bytes,
                    p.consumed,
                )
            })
            .collect::<Vec<_>>();
        let summary = goal.checkpoint.as_ref().map(|c| c.summary.as_str());
        let resident_bytes = pages
            .iter()
            .filter(|p| p.consumed && !p.released)
            .map(|p| p.body_bytes)
            .sum::<usize>();
        if std::env::var_os("LAD_BASELINE_EXPECT_RELEASED").is_some() {
            assert!(pages.iter().all(|page| page.consumed && page.released));
        }
        println!(
            "BASELINE {}",
            serde_json::json!({
                "time_ms": chrono::Utc::now().timestamp_millis(),
                "slice": goal.slice_seq,
                "state": goal.state,
                "reason": goal.status_reason,
                "checkpoint": summary,
                "usage": goal.used,
                "windows": windows,
                "resident": resident_bytes,
                "latest_request": s.latest_model_context.as_ref().and_then(|c| c.request_bytes),
            })
        );
        use desk_diagnose_core::goal::{GoalOwnerAction, GoalPauseReason, GoalState};
        if goal.state == GoalState::Paused(GoalPauseReason::Stalled) && goal.slice_seq == 3 {
            assert_eq!(
                windows,
                [
                    (Some(1), Some(48), 12_288, true),
                    (Some(49), Some(96), 12_288, true),
                    (Some(97), Some(144), 12_288, true),
                ]
            );
            if !retry_once {
                println!("BASELINE_OUTCOME paused_stalled");
                return;
            }
            assert!(!owner_retried);
            let retried = crate::agent_goal_store::apply_owner_action(
                &db,
                &run,
                &owner,
                "",
                &goal.goal_id,
                goal.state_version,
                GoalOwnerAction::RetryStalled,
                chrono::Utc::now(),
            )
            .await
            .unwrap()
            .unwrap();
            owner_retried = true;
            println!("BASELINE_OWNER_RETRY after_slice=3");
            resume_queued_goal(connections.clone(), db.clone(), &gate, retried)
                .await
                .unwrap();
            continue;
        }
        if goal.state != GoalState::Queued {
            if expect_context_limit {
                assert!(matches!(
                    goal.state,
                    GoalState::Blocked | GoalState::Paused(GoalPauseReason::ContextTooSmall)
                ));
                assert!(owner_retried && goal.slice_seq >= 4);
                assert!(
                    pages
                        .iter()
                        .filter(|p| p.consumed)
                        .map(|p| p.body_bytes)
                        .sum::<usize>()
                        < seed.len()
                );
                let (index, call) = s.conversation.iter().enumerate().rev().find_map(|(i, m)| {
                    m.tool_calls.iter().rev().find(|c| c.name == desk_diagnose_core::conversation_attachment::READ_ATTACHMENT_TOOL).map(|c| (i, c))
                }).expect("the goal must have requested an attachment page");
                let receipt = s.conversation[index + 1..]
                    .iter()
                    .take_while(|m| m.role != desk_diagnose_core::chat::ChatRole::Assistant)
                    .find(|m| {
                        m.role == desk_diagnose_core::chat::ChatRole::Tool
                            && m.tool_call_id.as_deref() == Some(&call.id)
                    })
                    .expect("the last read must have a matching tool receipt");
                assert_eq!(receipt.tool_ok, Some(false));
                assert!(
                    receipt
                        .text
                        .contains("Insufficient model context for this attachment page")
                );
                println!("BASELINE_OUTCOME attachment_context_limit");
                return;
            }
            assert!(owner_retried);
            assert_eq!(goal.state, GoalState::Completed);
            assert_eq!(goal.slice_seq, 6);
            assert_eq!(pages.len(), 6);
            for (index, page) in pages.iter().enumerate() {
                assert!(page.consumed);
                assert_eq!(page.body_bytes, 12_288);
                assert_eq!(page.input.start_line.unwrap_or(1), index * 48 + 1);
                assert_eq!(page.input.end_line, Some((index + 1) * 48));
            }
            let message = summary.unwrap();
            let start = message
                .find('{')
                .expect("expected counts in the final summary");
            let end = message.rfind('}').unwrap();
            let counts: serde_json::Value = serde_json::from_str(&message[start..=end]).unwrap();
            assert_eq!(
                counts["counts"],
                serde_json::json!([19, 14, 14, 12, 13, 20])
            );
            assert_eq!(counts["total"], 92);
            return;
        }
        resume_queued_goal(connections.clone(), db.clone(), &gate, goal)
            .await
            .unwrap();
    }
    panic!("the goal did not settle within its slice budget");
}
