use super::*;
use crate::ai_assistant::source_registry::{
    RegisteredSourceContract, SOURCE_CONTRACT_REVISION, registered_leaf,
};

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

fn load(skill_ref: &str, resource: Option<&str>) -> ToolCall {
    let args = match resource {
        Some(path) => json!({"skill_ref":skill_ref,"resource":path}),
        None => json!({"skill_ref":skill_ref}),
    };
    ToolCall {
        id: format!("load-{skill_ref}-{}", resource.unwrap_or("body")),
        name: LOAD_TOOL.into(),
        arguments_json: args.to_string(),
    }
}

fn call(resource: Option<&str>) -> ToolCall {
    load(NAME, resource)
}

fn bundled() -> SkillCatalog {
    SkillCatalog::default()
}

fn bundled_mut(state: &mut SkillState) -> &mut SkillSnapshot {
    state
        .uses
        .iter_mut()
        .find_map(|u| match u {
            SkillUse::Bundled(snapshot) => Some(snapshot),
            SkillUse::Stored(_) => None,
        })
        .unwrap()
}

fn entry(scope: SkillScope, name: &str, version: u64) -> StoredSkillEntry {
    StoredSkillEntry {
        skill_id: format!("id-{name}"),
        scope,
        name: name.into(),
        description: format!("Use {name}."),
        version,
        origin: SkillOrigin::Imported,
        size_bytes: 14 + 9,
        resources: vec![SkillFileMeta {
            path: "scripts/run.py".into(),
            kind: SkillResourceKind::Script,
            language: Some("python".into()),
            size_bytes: 9,
        }],
    }
}

fn content(name: &str, version: u64) -> StoredSkillContent {
    StoredSkillContent {
        skill_id: format!("id-{name}"),
        version,
        body: format!("# {name} v{version}\n"),
        allowed_tools: None,
        resources: vec![SkillResource {
            path: "scripts/run.py".into(),
            kind: SkillResourceKind::Script,
            language: Some("python".into()),
            text: "print(1)\n".into(),
        }],
        authored: None,
    }
}

#[test]
fn discovers_metadata_and_loads_instructions_and_reference_separately() {
    let mut state = SkillState::default();
    let before = state.project(1, &destination(), &bundled()).unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].text.contains("bundled/issue-analysis"));
    assert!(!before[0].text.contains("# Issue analysis"));
    let (receipt, root) = state.apply(1, &call(None), &bundled()).unwrap();
    assert!(root.is_none(), "a bundled receipt needs no stored root");
    assert!(!receipt.contains("# Issue analysis"));
    assert!(receipt.contains(CHECKLIST_PATH));
    let loaded = state.project(1, &destination(), &bundled()).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].role, ChatRole::System);
    assert!(loaded[1].text.contains("# Issue analysis"));
    state
        .apply(1, &call(Some(CHECKLIST_PATH)), &bundled())
        .unwrap();
    state
        .apply(
            1,
            &load("bundled/issue-analysis", Some(CHECKLIST_PATH)),
            &bundled(),
        )
        .unwrap();
    let loaded = state.project(1, &destination(), &bundled()).unwrap();
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[2].role, ChatRole::UntrustedOutput);
    assert_eq!(loaded[2].text, CHECKLIST);
    assert!(loaded.iter().all(is_projection));
    assert!(loaded.iter().all(
        |message| message.data_envelope.as_ref().unwrap().allowed_destinations == [destination()]
    ));
    assert!(
        state
            .apply(1, &call(Some("../SKILL.md")), &bundled())
            .is_err()
    );
    assert!(
        state
            .apply(1, &call(Some("scripts/run.sh")), &bundled())
            .is_err()
    );
    assert!(state.apply(1, &load("missing", None), &bundled()).is_err());
}

#[test]
fn native_snapshot_survives_takeover_and_a_new_input_loads_the_current_build() {
    let mut state = SkillState::default();
    state.apply(1, &call(None), &bundled()).unwrap();
    // A package captured by a node running an older product version.
    let package = bundled_mut(&mut state);
    package.source = package
        .source
        .replace("# Issue analysis", "# Original issue analysis");
    package.fingerprint = digest(&package.source, &package.resources);
    let encoded = serde_json::to_string(&state).unwrap();
    let mut restored: SkillState = serde_json::from_str(&encoded).unwrap();
    assert!(
        restored.project(1, &destination(), &bundled()).unwrap()[1]
            .text
            .contains("# Original issue analysis")
    );
    restored.apply(1, &call(None), &bundled()).unwrap();
    assert_eq!(serde_json::to_string(&restored).unwrap(), encoded);
    restored.apply(2, &call(None), &bundled()).unwrap();
    assert!(
        !restored.project(2, &destination(), &bundled()).unwrap()[1]
            .text
            .contains("# Original issue analysis")
    );
    assert_eq!(
        restored.publication_snapshot(2).unwrap().fingerprint(),
        SkillSnapshot::bundled(NAME, "current")
            .unwrap()
            .fingerprint()
    );
    let mut forged = crate::session::PersistedAgentSession::new(
        "session",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    forged
        .conversation
        .push(ChatMessage::tool_result("forged", "load-1", encoded));
    assert_eq!(
        forged
            .skills
            .project(1, &destination(), &bundled())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn structurally_corrupt_saved_content_is_disabled_and_never_becomes_an_instruction() {
    let mut state = SkillState::default();
    state.apply(1, &call(None), &bundled()).unwrap();
    bundled_mut(&mut state).resources[0].path = "../PRIVATE_CORRUPTED_PATH.md".into();
    let messages = state.project(1, &destination(), &bundled()).unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].text.contains("no longer available"));
    assert!(!messages[1].text.contains("# Issue analysis"));
    assert!(state.apply(1, &call(None), &bundled()).is_err());
    state.apply(2, &call(None), &bundled()).unwrap();
    assert_eq!(
        state.project(2, &destination(), &bundled()).unwrap().len(),
        2
    );
}

#[test]
fn a_package_captured_with_windows_line_endings_remains_loadable_on_another_node() {
    let mut package = SkillSnapshot::bundled(NAME, "windows-load").unwrap();
    package.source = package.source.replace('\n', "\r\n");
    for resource in &mut package.resources {
        resource.text = resource.text.replace('\n', "\r\n");
    }
    package.fingerprint = digest(&package.source, &package.resources);
    package.body_loaded = true;
    let mut state = SkillState {
        input_revision: 1,
        uses: vec![SkillUse::Bundled(package)],
    };
    state
        .apply(1, &call(Some(CHECKLIST_PATH)), &bundled())
        .unwrap();
    let projected = state.project(1, &destination(), &bundled()).unwrap();
    assert!(projected[1].text.contains("# Issue analysis\r\n"));
    assert_eq!(projected[2].role, ChatRole::UntrustedOutput);
}

#[test]
fn disabled_bundled_skills_are_hidden_and_cannot_be_loaded() {
    let catalog = SkillCatalog {
        disabled_bundled: vec![NAME.into()],
        ..Default::default()
    };
    let mut state = SkillState::default();
    let projected = state.project(1, &destination(), &catalog).unwrap();
    assert!(!projected[0].text.contains(NAME));
    assert!(state.apply(1, &call(None), &catalog).is_err());
}

#[test]
fn platform_and_personal_skills_use_their_own_roles_and_registered_labels() {
    let catalog = SkillCatalog {
        stored: vec![
            entry(SkillScope::Platform, "deploy", 3),
            entry(SkillScope::Personal, "notes", 1),
        ],
        loaded: vec![content("deploy", 3), content("notes", 1)],
        disabled_bundled: vec![],
    };
    let mut state = SkillState::default();
    let catalogue = &state.project(1, &destination(), &catalog).unwrap()[0];
    assert!(catalogue.text.contains("platform/deploy"));
    assert!(catalogue.text.contains("personal/notes"));
    state.apply(1, &load("deploy", None), &catalog).unwrap();
    state
        .apply(1, &load("personal/notes", None), &catalog)
        .unwrap();
    state
        .apply(1, &load("personal/notes", Some("scripts/run.py")), &catalog)
        .unwrap();
    assert!(
        state
            .apply(
                1,
                &load("personal/notes", Some("scripts/other.py")),
                &catalog
            )
            .is_err()
    );
    assert_eq!(
        state.loaded_stored(1),
        [("id-deploy".to_owned(), 3), ("id-notes".to_owned(), 1)]
    );
    let messages = state.project(1, &destination(), &catalog).unwrap();
    assert_eq!(messages.len(), 4);
    let (platform, personal, file) = (&messages[1], &messages[2], &messages[3]);
    assert_eq!(platform.role, ChatRole::System);
    assert!(platform.text.contains("PLATFORM SKILL deploy v3"));
    assert_eq!(personal.role, ChatRole::User);
    assert!(personal.text.contains("OWNER SKILL notes v1"));
    assert_eq!(file.role, ChatRole::UntrustedOutput);
    assert!(file.text.contains("print(1)"));
    assert!(messages.iter().all(is_projection));
    let contract = |message: &ChatMessage| {
        registered_leaf(
            message.data_envelope.as_ref().unwrap(),
            1,
            SOURCE_CONTRACT_REVISION,
        )
        .unwrap()
        .unwrap()
        .contract
    };
    assert_eq!(contract(platform), RegisteredSourceContract::PlatformSkill);
    assert_eq!(contract(personal), RegisteredSourceContract::PersonalSkill);
    assert_eq!(contract(file), RegisteredSourceContract::PersonalSkill);
}

#[test]
fn a_loaded_version_is_kept_for_the_input_and_the_catalogue_names_the_new_one() {
    let v1 = SkillCatalog {
        stored: vec![entry(SkillScope::Personal, "notes", 1)],
        loaded: vec![],
        disabled_bundled: vec![],
    };
    let mut state = SkillState::default();
    state.apply(1, &load("notes", None), &v1).unwrap();
    let v2 = SkillCatalog {
        stored: vec![entry(SkillScope::Personal, "notes", 2)],
        loaded: vec![content("notes", 1)],
        disabled_bundled: vec![],
    };
    let messages = state.project(1, &destination(), &v2).unwrap();
    assert!(messages[0].text.contains("keeps the loaded version 1"));
    assert!(messages[1].text.contains("# notes v1"));
    // Without the pinned content the skill is reported unavailable.
    let gone = SkillCatalog {
        loaded: vec![],
        ..v2.clone()
    };
    assert!(
        state.project(1, &destination(), &gone).unwrap()[1]
            .text
            .contains("no longer available")
    );
    state.apply(2, &load("notes", None), &v2).unwrap();
    assert_eq!(state.loaded_stored(2), [("id-notes".to_owned(), 2)]);
}

#[test]
fn loads_over_the_budget_return_a_receipt_and_change_nothing() {
    let mut big = entry(SkillScope::Platform, "huge", 1);
    big.size_bytes = (MAX_LOADED_BYTES + 1) as u64 + 9;
    let catalog = SkillCatalog {
        stored: vec![big],
        loaded: vec![],
        disabled_bundled: vec![],
    };
    let mut state = SkillState::default();
    let (receipt, root) = state.apply(1, &load("huge", None), &catalog).unwrap();
    assert!(
        root.is_some(),
        "a stored skill's receipt carries its version root"
    );
    assert!(receipt.contains("exceeds_budget"));
    assert!(state.uses(1).is_empty());
}

#[test]
fn catalogue_stays_within_its_budget() {
    let stored: Vec<_> = (0..200)
        .map(|i| {
            let mut e = entry(SkillScope::Platform, &format!("skill-{i}"), 1);
            e.description = "d".repeat(2000);
            e
        })
        .collect();
    let catalog = SkillCatalog {
        stored,
        ..Default::default()
    };
    let message = &SkillState::default()
        .project(1, &destination(), &catalog)
        .unwrap()[0];
    assert!(message.text.len() < MAX_CATALOG_BYTES + 512);
    assert!(message.text.contains("omitted"));
}

#[test]
fn loading_a_skill_cannot_expand_a_coordinator_or_worker_tool_authority() {
    use crate::{registry, session::PersistedAgentSession, subagent::AgentRole};
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session = PersistedAgentSession::new("session", "owner", "", 1, scope, "now");
    session.agent_role = AgentRole::Coordinator {
        workbench_id: "root".into(),
        issue_source: None,
        request_id: None,
    };
    let tool = session.skills.tool(1, &bundled());
    assert!(
        registry::lookup_for_session(std::slice::from_ref(&tool), LOAD_TOOL, &session).is_some()
    );
    session.skills.apply(1, &call(None), &bundled()).unwrap();
    assert!(!session.agent_role.allows_effect(ToolEffect::Mutating));
    assert!(!session.agent_role.allows_effect(ToolEffect::ExternalWrite));
    assert!(session.scope_snapshot.granted.is_empty());
    assert!(personal_visible(&session));
}

#[test]
fn publication_uses_the_native_skill_snapshot_and_rejects_replacement_instruction_text() {
    use crate::{
        external_tools::*, input_read_context::ReadContextSelection, issue_reply::ReplyOrigin,
    };
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut held =
        crate::session::PersistedAgentSession::new("worker", "owner", "", 1, scope, "now");
    held.input_revision = 1;
    held.current_turn_id = Some("turn".into());
    held.skills.apply(1, &call(None), &bundled()).unwrap();
    held.skills
        .apply(1, &call(Some(CHECKLIST_PATH)), &bundled())
        .unwrap();
    // Platform and personal skills never join a publication.
    let catalog = SkillCatalog {
        stored: vec![entry(SkillScope::Platform, "deploy", 1)],
        ..Default::default()
    };
    held.skills
        .apply(1, &load("deploy", None), &catalog)
        .unwrap();
    let access = IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: IssuePlatform::Gitea,
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            issue_number: 4,
            issue_id: 82,
            private: false,
            internal: false,
        },
    };
    let origin = ReplyOrigin {
        actor_id: "owner".into(),
        root_conversation_id: "root".into(),
        request_id: "request".into(),
        execution_conversation_id: "worker".into(),
        tool_call_id: "publish".into(),
        input_revision: 1,
        control_revision: held.control_revision,
        authorization_id: "grant".into(),
        authorization_revision: 1,
    };
    let mut selected = ReadContextSelection::capture(&[], &held.scope_snapshot).unwrap();
    selected.external_issues.push(access.clone());
    let read = ToolCall {
        id: "read".into(),
        name: TOOL_NAME.into(),
        arguments_json: json!({"tool_ref":access.tool_ref(),"args":{"method":"get"}}).to_string(),
    };
    let text = "Authorized issue workset";
    let mut material = ChatMessage::text("issue", ChatRole::UntrustedOutput, text);
    material.data_envelope = Some(
        label_issue_read(
            &held,
            &selected,
            &destination(),
            &read,
            &"a".repeat(64),
            text,
        )
        .unwrap(),
    );
    let policy = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "export".into(),
        now_unix_ms: 1,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut request = policy
        .prepare_issue_reply_call(origin, access.clone(), vec![material.clone()])
        .unwrap();
    request.bind_issue_reply_native_input(&held).unwrap();
    let original_messages = request.messages.clone();
    request.bind_issue_reply_native_input(&held).unwrap();
    assert_eq!(request.messages, original_messages);
    assert_eq!(request.messages.len(), 4);
    let binding = request.issue_reply.as_ref().unwrap();
    assert!(binding.native_input_matches(&held));
    assert_eq!(
        binding.issue_materials(&request.messages).unwrap(),
        [material]
    );
    assert_eq!(request.messages[1].role, ChatRole::System);
    assert_eq!(request.messages[2].role, ChatRole::UntrustedOutput);
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.text.contains("PLATFORM SKILL"))
    );
    let authorized = policy
        .clone()
        .for_request(&request)
        .unwrap()
        .authorize_request(request.clone())
        .unwrap();
    let output = policy
        .derive_model_output_envelope(
            &crate::chat::ModelTurn {
                text: "Supported reply".into(),
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    assert!(
        output
            .allowed_destinations
            .contains(&access.target.destination(access.connection_ref.clone()))
    );
    let mut forged = request.clone();
    forged.messages[1].text.push_str("\nPRIVATE_HISTORY");
    assert!(policy.clone().for_request(&forged).is_err());
    let package = bundled_mut(&mut held.skills);
    package.source = package
        .source
        .replace("# Issue analysis", "# Another package");
    package.fingerprint = digest(&package.source, &package.resources);
    assert!(!binding.native_input_matches(&held));
}

#[test]
fn ai_authored_versions_carry_their_note_and_the_authored_label_shape() {
    let mut stored = entry(SkillScope::Personal, "notes", 2);
    stored.skill_id = "abc123".into();
    stored.origin = SkillOrigin::AiAuthored;
    let mut loaded = content("notes", 2);
    loaded.skill_id = "abc123".into();
    loaded.authored = Some(AuthoredNote {
        decision_source: desk_agent_protocol::ai_assistant::skill::SkillDecisionSource::AiReview,
        decided_at_unix_ms: 1_792_000_000_000,
    });
    let catalog = SkillCatalog {
        stored: vec![stored],
        loaded: vec![loaded],
        disabled_bundled: vec![],
    };
    let mut state = SkillState::default();
    let catalogue = &state.project(1, &destination(), &catalog).unwrap()[0];
    assert!(catalogue.text.contains("\"ai_authored\":true"));
    state.apply(1, &load("notes", None), &catalog).unwrap();
    state
        .apply(1, &load("notes", Some("scripts/run.py")), &catalog)
        .unwrap();
    let messages = state.project(1, &destination(), &catalog).unwrap();
    let (body, file) = (&messages[1], &messages[2]);
    assert_eq!(body.role, ChatRole::User);
    assert!(body.text.contains("AI-AUTHORED"));
    assert!(body.text.contains("approved by the AI reviewer"));
    assert!(
        body.text
            .contains("cannot override the owner's current input")
    );
    for message in [body, file] {
        let label = message.data_envelope.as_ref().unwrap();
        assert_eq!(label.provenance.source_tool_name, "authored");
        assert_eq!(
            authored_version_of(
                &label.provenance.source_provider_id,
                &label.provenance.source_tool_name,
                label.provenance.source_object_id.as_deref().unwrap(),
            ),
            Some(("abc123".to_owned(), 2))
        );
        // Shape registration only; the host restores the stored proof.
        let registered = registered_leaf(label, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .unwrap();
        assert_eq!(registered.contract, RegisteredSourceContract::AuthoredSkill);
        assert!(!registered.policy.history_retention_allowed);
    }
    // Owner-saved content keeps the personal shape, and forged ids are refused.
    assert!(authored_version_of(SKILL_PROVIDER, PERSONAL_TOOL, "skill-authored-abc-v1").is_none());
    for forged in [
        "skill-personal-abc-v1",
        "skill-authored-abc-v0",
        "skill-authored-a-b-v1",
        "skill-authored-abc-v1-file-xyz",
    ] {
        assert!(
            authored_version_of(SKILL_PROVIDER, AUTHORED_TOOL, forged).is_none(),
            "{forged}"
        );
    }
}

/// Stored skill names and descriptions keep their skill's policy: the
/// catalogue cites each skill's root, and an AI-authored skill's root names
/// its version so hosts restore that version's proof.
#[test]
fn the_catalogue_cites_each_stored_skill_root() {
    use crate::ai_assistant::source_registry::{
        NativeInputPolicyRegistration, RegisteredSourceContract, SOURCE_CONTRACT_REVISION,
        stored_root_contract,
    };
    let mut authored = entry(SkillScope::Personal, "authored", 1);
    authored.skill_id = "idauthored".into();
    authored.origin = SkillOrigin::AiAuthored;
    let catalog = SkillCatalog {
        stored: vec![
            entry(SkillScope::Personal, "notes", 1),
            entry(SkillScope::Platform, "deploy", 1),
            authored,
        ],
        loaded: vec![],
        disabled_bundled: vec![],
    };
    let catalogue = SkillState::default()
        .project(1, &destination(), &catalog)
        .unwrap()
        .remove(0);
    let label = catalogue.data_envelope.clone().unwrap();
    assert!(
        label
            .envelope_id
            .starts_with(crate::model_message_labels::COMPOSED_RESULT_PREFIX)
    );
    let contracts = catalogue
        .stored_roots
        .iter()
        .map(|root| stored_root_contract(&root.label).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(catalogue.stored_roots.len(), 3);
    for contract in [
        RegisteredSourceContract::PersonalSkill,
        RegisteredSourceContract::PlatformSkill,
        RegisteredSourceContract::AuthoredSkill,
    ] {
        assert!(contracts.contains(&contract), "{contracts:?}");
    }
    let authored_root = catalogue
        .stored_roots
        .iter()
        .find(|root| {
            stored_root_contract(&root.label) == Some(RegisteredSourceContract::AuthoredSkill)
        })
        .unwrap();
    assert_eq!(
        authored_version_of(
            &authored_root.label.provenance.source_provider_id,
            &authored_root.label.provenance.source_tool_name,
            authored_root
                .label
                .provenance
                .source_object_id
                .as_deref()
                .unwrap(),
        ),
        Some(("idauthored".to_owned(), 1))
    );
    let audit = crate::sink_authorizer::SinkProjectionAudit {
        destination: destination(),
        envelope_ids: vec![label.envelope_id.clone()],
        total_bytes: catalogue.text.len(),
    };
    let snapshot = crate::model_egress::NativeModelInputSnapshot::capture_with_roots(
        &audit,
        std::slice::from_ref(&label),
        &[],
        &catalogue.stored_roots,
    )
    .unwrap();
    let registration =
        NativeInputPolicyRegistration::capture(&snapshot, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert_eq!(
        registration.leaves[0].as_ref().map(|leaf| leaf.contract),
        Some(RegisteredSourceContract::NativeControl)
    );
    // With only compiled skills the catalogue stays public server text.
    let bundled_only = SkillState::default()
        .project(1, &destination(), &bundled())
        .unwrap()
        .remove(0);
    assert!(bundled_only.stored_roots.is_empty());
    assert!(crate::model_egress::is_public_system_prompt(
        bundled_only.data_envelope.as_ref().unwrap()
    ));
}
