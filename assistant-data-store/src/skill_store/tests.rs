use super::*;
use crate::skill_package::parse_input;
use desk_agent_protocol::ai_assistant::skill::{SkillFileInput, SkillPackageInput};
use sea_orm::{Database, DatabaseConnection, Schema, TransactionTrait};

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

fn parsed(name: &str, body: &str) -> ParsedPackage {
    parse_input(
        &SkillPackageInput {
            zip_base64: None,
            skill_md: Some(format!(
                "---\nname: {name}\ndescription: about {name}\n---\n{body}"
            )),
            files: vec![SkillFileInput {
                path: "references/a.md".into(),
                text: format!("ref of {name}"),
            }],
        },
        None,
    )
    .unwrap()
}

fn package(name: &str, body: &str) -> SkillPackage {
    parsed(name, body).package
}

fn manual() -> VersionSource {
    VersionSource::manual(SkillOrigin::Imported, None)
}

#[tokio::test]
async fn create_update_and_delete_keep_versions_and_counters_consistent() {
    let db = db().await;
    let scope = personal_scope("7");
    let created = create_on(&db, &scope, &package("notes", "v1"), &manual(), 1)
        .await
        .unwrap();
    assert_eq!(
        (created.current_version, created.revision, created.enabled),
        (1, 1, true)
    );
    assert!(matches!(
        create_on(&db, &scope, &package("notes", "again"), &manual(), 2).await,
        Err(SkillStoreError::NameTaken)
    ));
    let edited = update_on(
        &db,
        &scope,
        &created.skill_id,
        1,
        None,
        Some((
            &package("notes", "v2"),
            &VersionSource::manual(SkillOrigin::Edited, None),
        )),
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        (edited.current_version, edited.origin),
        (2, SkillOrigin::Edited)
    );
    assert!(matches!(
        update_on(&db, &scope, &created.skill_id, 1, Some(false), None, 4).await,
        Err(SkillStoreError::Conflict)
    ));
    let detail = detail_on(&db, &scope, &created.skill_id).await.unwrap();
    assert!(detail.skill_md.ends_with("v2"));
    assert_eq!(detail.versions.len(), 2);
    assert_eq!(detail.files[0].text, "ref of notes");
    // Another scope cannot read or change it.
    assert!(matches!(
        detail_on(&db, PLATFORM_SCOPE, &created.skill_id).await,
        Err(SkillStoreError::NotFound)
    ));
    let disabled = update_on(&db, &scope, &created.skill_id, 2, Some(false), None, 5)
        .await
        .unwrap();
    assert!(
        catalog_on(&db, std::slice::from_ref(&scope))
            .await
            .unwrap()
            .is_empty()
    );
    delete_on(&db, &scope, &created.skill_id, disabled.revision)
        .await
        .unwrap();
    let counter = scope::Entity::find()
        .filter(scope::Column::ScopeKey.eq(&scope))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((counter.skill_count, counter.enabled_count), (0, 0));
}

#[tokio::test]
async fn new_versions_cannot_rename_and_old_versions_are_pruned() {
    let db = db().await;
    let created = create_on(&db, PLATFORM_SCOPE, &package("guide", "v1"), &manual(), 1)
        .await
        .unwrap();
    assert!(matches!(
        update_on(
            &db,
            PLATFORM_SCOPE,
            &created.skill_id,
            1,
            None,
            Some((&package("renamed", "x"), &manual())),
            2
        )
        .await,
        Err(SkillStoreError::Conflict)
    ));
    let mut revision = 1;
    for i in 2..=13 {
        revision = update_on(
            &db,
            PLATFORM_SCOPE,
            &created.skill_id,
            revision,
            None,
            Some((&package("guide", &format!("v{i}")), &manual())),
            i,
        )
        .await
        .unwrap()
        .revision;
    }
    let detail = detail_on(&db, PLATFORM_SCOPE, &created.skill_id)
        .await
        .unwrap();
    assert_eq!(detail.versions.len() as i64, RETAINED_VERSIONS);
    assert_eq!(detail.versions[0].version, 13);
    assert!(
        version_on(&db, &created.skill_id, 3)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn scope_limits_are_enforced() {
    let db = db().await;
    let scope = personal_scope("1");
    for i in 0..MAX_ENABLED_PER_SCOPE {
        create_on(&db, &scope, &package(&format!("s{i}"), "x"), &manual(), 1)
            .await
            .unwrap();
    }
    assert!(matches!(
        create_on(&db, &scope, &package("over", "x"), &manual(), 1).await,
        Err(SkillStoreError::Limit(_))
    ));
}

#[tokio::test]
async fn catalog_lists_enabled_skills_of_requested_scopes_without_contents() {
    let db = db().await;
    create_on(&db, PLATFORM_SCOPE, &package("shared", "p"), &manual(), 1)
        .await
        .unwrap();
    create_on(
        &db,
        &personal_scope("1"),
        &package("mine", "a"),
        &manual(),
        1,
    )
    .await
    .unwrap();
    create_on(
        &db,
        &personal_scope("2"),
        &package("theirs", "b"),
        &manual(),
        1,
    )
    .await
    .unwrap();
    let rows = catalog_on(&db, &[PLATFORM_SCOPE.into(), personal_scope("1")])
        .await
        .unwrap();
    let names: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.scope)).collect();
    assert_eq!(
        names,
        [
            ("mine", SkillScope::Personal),
            ("shared", SkillScope::Platform)
        ]
    );
    assert_eq!(rows[0].resources[0].path, "references/a.md");
}

#[tokio::test]
async fn account_cleanup_removes_only_that_accounts_personal_skills() {
    let db = db().await;
    let txn = db.begin().await.unwrap();
    create_on(
        &txn,
        &personal_scope("1"),
        &package("mine", "a"),
        &manual(),
        1,
    )
    .await
    .unwrap();
    create_on(&txn, PLATFORM_SCOPE, &package("shared", "p"), &manual(), 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    delete_personal_skills_on(&db, "1").await.unwrap();
    assert!(list_on(&db, &personal_scope("1")).await.unwrap().is_empty());
    assert_eq!(list_on(&db, PLATFORM_SCOPE).await.unwrap().len(), 1);
    assert_eq!(version::Entity::find().all(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn saves_attach_a_live_scan_of_the_same_scope_only() {
    let db = db().await;
    let scope = personal_scope("1");
    let scan_id = save_scan_on(&db, &scope, "{\"verdict\":\"clean\"}", 100)
        .await
        .unwrap();
    assert!(
        scan_summary_on(&db, PLATFORM_SCOPE, &scan_id, 100)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        scan_summary_on(&db, &scope, &scan_id, 100 + SCAN_TTL_MS + 1)
            .await
            .unwrap()
            .is_none()
    );
    let saved = save_manual_on(
        &db,
        &scope,
        &parsed("scanned", "x"),
        None,
        SkillOrigin::Imported,
        Some(&scan_id),
        200,
    )
    .await
    .unwrap();
    let row = version_on(&db, &saved.skill_id, 1).await.unwrap().unwrap();
    assert_eq!(
        row.scan_summary_json.as_deref(),
        Some("{\"verdict\":\"clean\"}")
    );
}

mod service_tests {
    use super::*;
    use crate::skill_store::service::*;
    use desk_agent_protocol::ai_assistant::skill::{
        SkillCreate, SkillFileChange, SkillManualOrigin, SkillUpdate,
    };

    fn input(body: &str) -> SkillPackageInput {
        SkillPackageInput {
            zip_base64: None,
            skill_md: Some(format!("---\nname: notes\ndescription: d\n---\n{body}")),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn uploads_preview_save_and_feed_the_request_catalog() {
        let db = db().await;
        let scope = personal_scope("5");
        let (_, preview) = preview_on(&db, &scope, None, &input("one\n"))
            .await
            .unwrap();
        assert_eq!(preview.diff[0].change, SkillFileChange::Added);
        let created = create_from_input_on(
            &db,
            &scope,
            &SkillCreate {
                package: input("one\n"),
                scan_id: None,
            },
            1,
        )
        .await
        .unwrap();
        let id = created.skill.skill_id.clone();
        let (_, preview) = preview_on(&db, &scope, Some(&id), &input("two\n"))
            .await
            .unwrap();
        assert_eq!(preview.diff[0].change, SkillFileChange::Modified);
        let renamed = SkillPackageInput {
            skill_md: Some("---\nname: other\ndescription: d\n---\nx".into()),
            ..input("")
        };
        let error = preview_on(&db, &scope, Some(&id), &renamed)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), SkillErrorKind::Invalid);
        let edited = update_from_input_on(
            &db,
            &scope,
            &id,
            &SkillUpdate {
                expected_revision: 1,
                enabled: None,
                package: Some(input("two\n")),
                origin: Some(SkillManualOrigin::Edited),
                scan_id: None,
            },
            2,
        )
        .await
        .unwrap();
        assert_eq!(edited.skill.origin, SkillOrigin::Edited);
        let stale = update_from_input_on(
            &db,
            &scope,
            &id,
            &SkillUpdate {
                expected_revision: 1,
                enabled: Some(false),
                package: None,
                origin: None,
                scan_id: None,
            },
            3,
        )
        .await
        .unwrap_err();
        assert_eq!(stale.kind(), SkillErrorKind::Conflict);

        // The catalog shows the current version and returns pinned content.
        let catalog = host_catalog_on(
            &db,
            &visible_scopes("5", true),
            &[(id.clone(), 1)],
            vec!["x".into()],
        )
        .await
        .unwrap();
        assert_eq!(catalog.stored[0].version, 2);
        assert_eq!(catalog.loaded[0].body, "one\n");
        assert_eq!(catalog.disabled_bundled, ["x"]);
        // Without personal visibility neither the entry nor its content is returned.
        let external = host_catalog_on(&db, &visible_scopes("5", false), &[(id, 1)], vec![])
            .await
            .unwrap();
        assert!(external.stored.is_empty() && external.loaded.is_empty());
    }
}
