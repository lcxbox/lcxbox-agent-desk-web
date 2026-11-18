//! Handler cancellation uses real vault/quota locks, without a desktop or network.
use super::*;
use actix_session::{Session, SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, cookie::Key, middleware::from_fn, test};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use std::{io::Read, time::Duration};

async fn login(session: Session) -> HttpResponse {
    session
        .set_current_user(&CurrentUser::new_admin("owner"))
        .unwrap();
    HttpResponse::Ok().finish()
}

#[actix_web::test]
async fn cancelled_export_releases_locks_and_cleanup_does_not_rewrite_response() {
    let directory = tempfile::tempdir().unwrap();
    let settings = SharedSettings::from(crate::model::settings::Settings::for_test_config(
        &directory.path().join("config"),
    ));
    let root = settings.read().await.paths().data_root().to_path_buf();
    let vault = desk_file_recovery::Vault::open(&root).unwrap();
    let scope = desk_file_recovery::Scope {
        authority: "a".repeat(64),
        device: "fixture".into(),
        owner: "owner".into(),
        os_user: crate::file_recovery_service::platform_user::current().unwrap(),
    };
    let record = crate::file_recovery_service::test_support::backup(
        &mut vault.lock().unwrap(),
        &scope,
        "conversation",
        "operation",
        b"before",
    );
    let now = record.created_at_ms;
    let app = test::init_service(
        App::new()
            .wrap(SessionMiddleware::new(
                CookieSessionStore::default(),
                Key::generate(),
            ))
            .app_data(web::Data::new(settings))
            .route("/login", web::post().to(login))
            .service(
                web::scope("/api/desk")
                    .wrap(from_fn(crate::controller::user::enforce_device_scope))
                    .service(export_local_file_recovery)
                    .service(discard_local_file_recovery),
            ),
    )
    .await;
    let response =
        test::call_service(&app, test::TestRequest::post().uri("/login").to_request()).await;
    let cookie = response.response().cookies().next().unwrap().into_owned();
    let export_request = || {
        test::TestRequest::post()
            .uri("/api/desk/file-recovery/export")
            .cookie(cookie.clone())
            .peer_addr("127.0.0.1:1234".parse().unwrap())
            .insert_header((header::HOST, "localhost"))
            .insert_header((header::ORIGIN, "http://localhost"))
            .set_json(serde_json::json!({"recovery_id": record.id}))
            .to_request()
    };

    // The production blocking task locks the vault before asking for quota.
    // Hold quota first so vault contention proves the handler reached this point.
    // The probe takes the vault lock for a moment itself; an export that tries
    // the vault at that instant answers "busy" at once and is sent again. Any
    // other early answer means the export did not wait for the quota lock.
    let quota = desk_file_recovery::quota::DeviceQuota::open(&root).unwrap();
    let held = quota.lock().unwrap();
    let mut request = Box::pin(test::call_service(&app, export_request()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let answered = tokio::select! {
                response = &mut request => Some(response),
                _ = tokio::time::sleep(Duration::from_millis(5)) => None,
            };
            match answered {
                Some(response) => {
                    let body: serde_json::Value = test::read_body_json(response).await;
                    assert_eq!(
                        body["data"], "busy",
                        "export bypassed the held quota lock: {body}"
                    );
                    request = Box::pin(test::call_service(&app, export_request()));
                }
                None => {
                    if vault.try_lock().unwrap().is_none() {
                        break;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    // Dropping the HTTP handler future cannot stop an already-running blocking
    // export. Releasing quota lets it finish and drop both guards normally.
    drop(request);
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(locked) = vault.try_lock().unwrap() {
                assert!(
                    locked
                        .entry_material(&scope, "conversation", &record.id, 0, now)
                        .is_ok()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        test::call_service(&app, export_request()),
    )
    .await
    .expect("export locks leaked after cancellation");
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    // No body consumption is required to release storage locks.
    drop(response);
    let completed = test::call_service(&app, export_request()).await;
    assert_eq!(
        completed.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    let discard = test::call_service(&app, test::TestRequest::post()
        .uri("/api/desk/file-recovery/discard").cookie(cookie.clone())
        .peer_addr("127.0.0.1:1234".parse().unwrap())
        .insert_header((header::HOST, "localhost"))
        .insert_header((header::ORIGIN, "http://localhost"))
        .set_json(serde_json::json!({"recovery_id": record.id, "conversation_id": "conversation", "confirmed": true}))
        .to_request()).await;
    let discarded: serde_json::Value = test::read_body_json(discard).await;
    assert_eq!(discarded["success"], true, "{discarded}");
    let bytes = test::read_body(completed).await;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut before = String::new();
    archive
        .by_name("before.txt")
        .unwrap()
        .read_to_string(&mut before)
        .unwrap();
    assert_eq!(before, "before");
    let refused: serde_json::Value =
        test::read_body_json(test::call_service(&app, export_request()).await).await;
    assert_eq!(refused["success"], false);
    assert_eq!(refused["data"], "material_cleaned");
}

#[actix_web::test]
async fn large_exports_stream_every_chunk_under_the_declared_length() {
    let directory = tempfile::tempdir().unwrap();
    let settings = SharedSettings::from(crate::model::settings::Settings::for_test_config(
        &directory.path().join("config"),
    ));
    let root = settings.read().await.paths().data_root().to_path_buf();
    let vault = desk_file_recovery::Vault::open(&root).unwrap();
    let scope = desk_file_recovery::Scope {
        authority: "a".repeat(64),
        device: "fixture".into(),
        owner: "owner".into(),
        os_user: crate::file_recovery_service::platform_user::current().unwrap(),
    };
    // Incompressible letters make a package of several chunks.
    let mut state = 0x9e37_79b9_u32;
    let content: Vec<u8> = (0..3_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'a' + (state % 26) as u8
        })
        .collect();
    let record = crate::file_recovery_service::test_support::backup(
        &mut vault.lock().unwrap(),
        &scope,
        "conversation",
        "operation",
        &content,
    );
    let app = test::init_service(
        App::new()
            .wrap(SessionMiddleware::new(
                CookieSessionStore::default(),
                Key::generate(),
            ))
            .app_data(web::Data::new(settings))
            .route("/login", web::post().to(login))
            .service(
                web::scope("/api/desk")
                    .wrap(from_fn(crate::controller::user::enforce_device_scope))
                    .service(export_local_file_recovery),
            ),
    )
    .await;
    let response =
        test::call_service(&app, test::TestRequest::post().uri("/login").to_request()).await;
    let cookie = response.response().cookies().next().unwrap().into_owned();
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/desk/file-recovery/export")
            .cookie(cookie)
            .peer_addr("127.0.0.1:1234".parse().unwrap())
            .insert_header((header::HOST, "localhost"))
            .insert_header((header::ORIGIN, "http://localhost"))
            .set_json(serde_json::json!({"recovery_id": record.id, "entry": 0}))
            .to_request(),
    )
    .await;
    // The declared size becomes the Content-Length header on the wire.
    let actix_web::body::BodySize::Sized(declared) =
        actix_web::body::MessageBody::size(response.response().body())
    else {
        panic!("export must declare its full length")
    };
    let declared = declared as usize;
    let bytes = test::read_body(response).await;
    assert_eq!(bytes.len(), declared);
    assert!(declared > 2 * desk_agent_protocol::file_recovery::MAX_EXPORT_CHUNK_BYTES);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut before = Vec::new();
    archive
        .by_name("before.txt")
        .unwrap()
        .read_to_end(&mut before)
        .unwrap();
    assert_eq!(before, content);
}
