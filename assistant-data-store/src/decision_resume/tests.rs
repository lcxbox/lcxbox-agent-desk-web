use super::*;
use desk_diagnose_core::proposal_review::DecisionPause;
use sea_orm::{Database, DatabaseConnection, Schema};

async fn db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    db
}

fn paused(kind: DecisionKind, id: &str) -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "chat",
        "1",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-14T00:00:00Z",
    );
    session.input_revision = 2;
    session.decision_pause = Some(DecisionPause {
        kind,
        id: id.into(),
        turn_id: "turn".into(),
        call_id: "call".into(),
        input_revision: 2,
        requested_at_unix_ms: 1,
    });
    session
}

#[tokio::test]
async fn a_record_leaves_pending_exactly_once() {
    let db = db().await;
    let session = paused(DecisionKind::Directory, "dir");
    assert!(paused_on(&session, DecisionKind::Directory, "dir"));
    insert_on(&db, &session, "", DecisionKind::Directory, "dir", 10)
        .await
        .unwrap();
    let pending = pending_on(&db, 0, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input_revision, 2);
    let id = pending[0].id;
    assert!(transition_on(&db, id, PENDING, STARTED, 11).await.unwrap());
    // A second instance loses the same claim.
    assert!(!transition_on(&db, id, PENDING, STARTED, 11).await.unwrap());
    assert!(pending_on(&db, 0, 10).await.unwrap().is_empty());
    assert_eq!(started_on(&db, 10).await.unwrap().len(), 1);
    assert!(transition_on(&db, id, STARTED, DONE, 12).await.unwrap());
    assert_eq!(
        find_on(&db, DecisionKind::Directory, "dir")
            .await
            .unwrap()
            .unwrap()
            .state,
        DONE
    );
}

#[tokio::test]
async fn withdrawing_supersedes_unclaimed_resumes_and_frees_the_proposal() {
    let db = db().await;
    let session = paused(DecisionKind::Skill, "p1");
    insert_on(&db, &session, "", DecisionKind::Skill, "p1", 10)
        .await
        .unwrap();
    let pause = session.decision_pause.clone().unwrap();
    withdraw_on(&db, &session, &pause, 20).await.unwrap();
    let record = find_on(&db, DecisionKind::Skill, "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SUPERSEDED);
    assert!(pending_on(&db, 0, 10).await.unwrap().is_empty());
    // A running turn is never paused, and another request is not this one.
    let mut running = paused(DecisionKind::Skill, "p1");
    running.turn_state = desk_diagnose_core::session::TurnState::Running;
    assert!(!paused_on(&running, DecisionKind::Skill, "p1"));
    assert!(!paused_on(&session, DecisionKind::Skill, "p2"));
}
