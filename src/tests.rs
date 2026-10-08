use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};

use actix_web::{
    App, FromRequest as _, HttpResponse, HttpServer,
    dev::Payload,
    test,
    web::{self, Bytes, Data},
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{
    config::{Config, Destination, Route, Secret, Source},
    discord, github,
    queue::Queue,
    routing, worker,
};

fn config() -> Config {
    Config {
        bind: "127.0.0.1:8080".into(),
        sources: BTreeMap::from([(
            "org".into(),
            Source {
                secret: Secret {
                    env: None,
                    file: None,
                },
                repositories: vec!["actix/actix-web".into()],
                key: "test-secret".into(),
            },
        )]),
        destinations: BTreeMap::from([(
            "web".into(),
            Destination {
                webhook: Secret {
                    env: None,
                    file: None,
                },
                url: "http://127.0.0.1:1".into(),
            },
        )]),
        routes: vec![Route {
            name: "web".into(),
            repository: "actix/actix-web".into(),
            tag_prefix: Some("web-".into()),
            exclude_tags: Vec::new(),
            include_prereleases: false,
            destinations: vec!["web".into()],
        }],
    }
}

fn event() -> Value {
    serde_json::from_str(include_str!("../examples/release.json")).unwrap()
}
fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[actix_web::test]
async fn signatures_cover_exact_bytes_and_chunked_payloads() {
    for chunks in [
        vec![Bytes::from_static(b"{ \"x\": 1 }")],
        vec![
            Bytes::from_static(b"{ "),
            Bytes::from_static(b"\"x\":"),
            Bytes::from_static(b" 1 }"),
        ],
    ] {
        let exact = chunks.concat();
        for (signature, success) in [
            (Some(sign(&exact)), true),
            (Some(sign(b"{\"x\":1}")), false),
            (Some("sha256=invalid".into()), false),
            (Some(format!("sha256={}", "00".repeat(32))), false),
            (None, false),
        ] {
            let mut builder = test::TestRequest::post()
                .param("source", "org")
                .insert_header(("content-type", "application/json"))
                .app_data(Data::new(config()));
            if let Some(signature) = signature {
                builder = builder.insert_header(("x-hub-signature-256", signature));
            }
            let request = builder.to_http_request();
            let stream = futures_util::stream::iter(
                chunks
                    .clone()
                    .into_iter()
                    .map(Ok::<_, actix_web::error::PayloadError>),
            );
            let mut payload = Payload::from(Box::pin(stream)
                as std::pin::Pin<
                    Box<
                        dyn futures_util::Stream<
                                Item = Result<Bytes, actix_web::error::PayloadError>,
                            >,
                    >,
                >);
            let result = github::SignedBody::from_request(&request, &mut payload).await;
            assert_eq!(result.is_ok(), success);
            if let Err(error) = result {
                assert_eq!(
                    actix_web::Error::from(error)
                        .as_response_error()
                        .status_code(),
                    401
                );
            }
        }
    }
}

#[actix_web::test]
async fn incoming_filters_authorizes_limits_and_deduplicates() {
    let queue = Queue::default();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(config()))
            .app_data(Data::new(queue.clone()))
            .app_data(Data::new(worker::WorkerHealth(Arc::new(AtomicBool::new(
                true,
            )))))
            .configure(github::endpoints),
    )
    .await;
    let mut wrong = event();
    wrong["repository"]["full_name"] = json!("other/repo");
    let mut edited = event();
    edited["action"] = json!("edited");
    let mut prerelease = event();
    prerelease["release"]["prerelease"] = json!(true);
    let mut unmatched = event();
    unmatched["release"]["tag_name"] = json!("other-v1");
    for (id, kind, value, expected) in [
        ("ping", "ping", json!({}), 200),
        ("wrong", "release", wrong, 403),
        ("edited", "release", edited, 204),
        ("push", "push", event(), 204),
        ("pre", "release", prerelease, 202),
        ("unmatched", "release", unmatched, 202),
        ("first", "release", event(), 202),
        ("first", "release", event(), 202),
        ("second", "release", event(), 202),
    ] {
        let body = value.to_string();
        let request = test::TestRequest::post()
            .uri("/webhooks/github/org")
            .insert_header(("content-type", "application/json"))
            .insert_header(("x-hub-signature-256", sign(body.as_bytes())))
            .insert_header(("x-github-delivery", id))
            .insert_header(("x-github-event", kind))
            .set_payload(body)
            .to_request();
        assert_eq!(
            test::call_service(&app, request).await.status(),
            expected,
            "{id}"
        );
    }
    assert!(queue.claim().await.is_some());
    assert!(queue.claim().await.is_none());
    let huge = json!({"body": "x".repeat(github::MAX_BODY)}).to_string();
    let request = test::TestRequest::post()
        .uri("/webhooks/github/org")
        .insert_header(("content-type", "application/json"))
        .insert_header(("x-hub-signature-256", sign(huge.as_bytes())))
        .set_payload(huge)
        .to_request();
    assert_eq!(test::call_service(&app, request).await.status(), 413);
    for path in ["/healthz", "/readyz"] {
        assert_eq!(
            test::call_service(&app, test::TestRequest::get().uri(path).to_request())
                .await
                .status(),
            200
        );
    }
}

#[actix_web::test]
async fn incoming_excludes_case_insensitive_regexes_before_queueing_announcements() {
    for (tag, expected_job) in [
        ("actix-web-lab-derive-v0.1.0", false),
        ("actix-web-lab-derive-", false),
        ("actix-web-lab-v0.27.0-internal", false),
        ("actix-web-lab-v0.27.0-TEST", false),
        ("actix-web-lab-v0.27.0", true),
        ("actix-web-lab-Derive-v0.1.0", false),
        ("actix-web-lab-test-v0.1.0", true),
        ("actix-web-lab-v0.27.0-internal-more", true),
        ("other-actix-web-lab-v0.27.0", false),
    ] {
        let mut config = config();
        config.sources.get_mut("org").unwrap().repositories = vec!["robjtede/actix-web-lab".into()];
        config.routes = vec![
            toml::from_str(
                r#"
            name = "lab"
            repository = "robjtede/actix-web-lab"
            tag_prefix = "actix-web-lab-"
            exclude_tags = ["^actix-web-lab-derive-.*$", "-(internal|test)$"]
            destinations = ["web"]
            "#,
            )
            .unwrap(),
        ];

        let queue = Queue::default();
        let app = test::init_service(
            App::new()
                .app_data(Data::new(config))
                .app_data(Data::new(queue.clone()))
                .configure(github::endpoints),
        )
        .await;

        let mut value = event();
        value["repository"]["full_name"] = json!("robjtede/actix-web-lab");
        value["release"]["tag_name"] = json!(tag);
        let body = value.to_string();
        let request = test::TestRequest::post()
            .uri("/webhooks/github/org")
            .insert_header(("content-type", "application/json"))
            .insert_header(("x-hub-signature-256", sign(body.as_bytes())))
            .insert_header(("x-github-delivery", "tag-filter"))
            .insert_header(("x-github-event", "release"))
            .set_payload(body)
            .to_request();

        assert_eq!(
            test::call_service(&app, request).await.status(),
            202,
            "{tag}"
        );
        assert_eq!(queue.claim().await.is_some(), expected_job, "{tag}");
        assert!(queue.claim().await.is_none(), "{tag}");
    }
}

#[actix_web::test]
async fn routes_apply_tag_exclusions_per_route() {
    let mut config = config();
    config.routes[0].tag_prefix = None;
    config.routes.push(
        toml::from_str(
            r#"
            name = "filtered"
            repository = "actix/actix-web"
            exclude_tags = ["^actix-web-lab-derive-.*$"]
            destinations = ["web", "another"]
            "#,
        )
        .unwrap(),
    );

    let mut release: github::Release = serde_json::from_value(event()["release"].clone()).unwrap();
    release.tag_name = "ACTIX-WEB-LAB-DERIVE-v0.1.0".into();

    assert_eq!(
        routing::destinations(&config, "actix/actix-web", &release),
        BTreeMap::from([("web".into(), vec!["web".into()])])
    );
}

#[actix_web::test]
async fn routes_union_destinations_and_include_prereleases_per_route() {
    let mut config = config();
    config.routes.push(Route {
        name: "all".into(),
        repository: "actix/actix-web".into(),
        tag_prefix: None,
        exclude_tags: Vec::new(),
        include_prereleases: true,
        destinations: vec!["web".into(), "another".into()],
    });
    let mut release: github::Release = serde_json::from_value(event()["release"].clone()).unwrap();
    let matched = routing::destinations(&config, "actix/actix-web", &release);
    assert_eq!(matched.len(), 2);
    assert_eq!(matched["web"], ["web", "all"]);
    release.prerelease = true;
    assert_eq!(
        routing::destinations(&config, "actix/actix-web", &release)["web"],
        ["all"]
    );
    release.tag_name = "unprefixed".into();
    assert_eq!(
        routing::destinations(&config, "actix/actix-web", &release).len(),
        2
    );
    assert!(routing::destinations(&config, "actix/other", &release).is_empty());
}

#[actix_web::test]
async fn embed_title_uses_only_the_release_name_and_links_to_the_release() {
    let mut release: github::Release = serde_json::from_value(event()["release"].clone()).unwrap();

    let message = discord::render("actix/actix-web", &release);
    let embed = &message.payload["embeds"][0];
    assert_eq!(embed["title"], "Actix Web 4.0.0");
    assert_eq!(embed["url"], release.html_url);
    assert!(embed.get("timestamp").is_none());

    release.name = None;
    let message = discord::render("actix/actix-web", &release);
    assert_eq!(message.payload["embeds"][0]["title"], "web-v4.0.0");
}

#[actix_web::test]
async fn unicode_notes_are_complete_and_embed_limits_hold() {
    let mut release: github::Release = serde_json::from_value(event()["release"].clone()).unwrap();
    for notes in [
        String::new(),
        "x".repeat(4096),
        "🚀 **change**\n".repeat(2000),
    ] {
        release.body = Some(notes.clone());
        release.name = Some("🚀".repeat(400));
        let message = discord::render(&"r".repeat(500), &release);
        let embed = &message.payload["embeds"][0];
        let lengths = [
            embed["title"].as_str().unwrap().encode_utf16().count(),
            embed["description"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count(),
            embed["author"]["name"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count(),
        ];
        assert!(
            lengths[0] <= 256
                && lengths[1] <= 4096
                && lengths[2] <= 256
                && lengths.iter().sum::<usize>() <= 6000
        );
        assert_eq!(message.payload["allowed_mentions"]["parse"], json!([]));
        if notes.encode_utf16().count() > 4096 {
            assert_eq!(message.notes.as_deref(), Some(notes.as_str()));
        } else {
            assert!(message.notes.is_none());
        }
    }
}

#[actix_web::test]
async fn fake_discord_checks_payload_attachments_and_failure_classes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Data::new(std::sync::Mutex::new(Vec::<(String, String, Bytes)>::new()));
    let capture = captured.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(capture.clone())
            .default_service(web::post().to(
                |req: actix_web::HttpRequest,
                 body: Bytes,
                 capture: Data<std::sync::Mutex<Vec<(String, String, Bytes)>>>| async move {
                    assert_eq!(req.query_string(), "wait=true");
                    capture.lock().unwrap().push((
                        req.path().to_owned(),
                        req.headers()
                            .get("content-type")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned(),
                        body,
                    ));
                    match req.path() {
                        "/limited" => HttpResponse::TooManyRequests()
                            .insert_header(("retry-after", "3"))
                            .json(json!({"retry_after": 4.2})),
                        "/transient" => HttpResponse::InternalServerError().finish(),
                        "/permanent" => HttpResponse::NotFound().finish(),
                        "/unconfirmed" => HttpResponse::NoContent().finish(),
                        _ => HttpResponse::Ok().json(json!({"id": "123"})),
                    }
                },
            ))
    })
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let client = worker::client().unwrap();
    let mut release: github::Release = serde_json::from_value(event()["release"].clone()).unwrap();
    let message = discord::render("actix/actix-web", &release);
    for path in ["ok", "limited", "transient", "permanent", "unconfirmed"] {
        let outcome = worker::send(&client, &format!("http://{address}/{path}"), &message, 1).await;
        assert!(match path {
            "ok" => matches!(outcome, worker::Outcome::Sent),
            "limited" => matches!(
                outcome,
                worker::Outcome::Retry {
                    delay: 6,
                    rate_limited: true,
                    ..
                }
            ),
            "transient" | "unconfirmed" => matches!(
                outcome,
                worker::Outcome::Retry {
                    rate_limited: false,
                    ..
                }
            ),
            _ => matches!(outcome, worker::Outcome::Failed(_)),
        });
    }
    release.body = Some("🚀\n".repeat(3000));
    let long = discord::render("actix/actix-web", &release);
    assert!(matches!(
        worker::send(&client, &format!("http://{address}/upload"), &long, 1).await,
        worker::Outcome::Sent
    ));
    {
        let requests = captured.lock().unwrap();
        let plain: Value = serde_json::from_slice(&requests[0].2).unwrap();
        assert_eq!(plain, message.payload);
        assert!(requests[5].1.starts_with("multipart/form-data; boundary="));
        let multipart = std::str::from_utf8(&requests[5].2).unwrap();
        assert!(multipart.contains("name=\"payload_json\""));
        assert!(multipart.contains("filename=\"release-notes.md\""));
        assert!(multipart.contains(release.body.as_deref().unwrap()));
        assert!(multipart.contains(&long.payload.to_string()));
    }
    handle.stop(true).await;
}

#[actix_web::test]
async fn config_validates_references_and_redacts_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    let hook = dir.path().join("hook");
    std::fs::write(&key, "test-secret\n").unwrap();
    std::fs::write(
        &hook,
        "https://discord.com/api/webhooks/123/private-token\n",
    )
    .unwrap();
    let input = format!(
        r#"
        [sources.org]
        repositories = ["actix/actix-web"]
        secret = {{ file = {:?} }}
        [destinations.web]
        webhook = {{ file = {:?} }}
        [[routes]]
        name = "web"
        repository = "actix/actix-web"
        destinations = ["web"]
    "#,
        key.to_str().unwrap(),
        hook.to_str().unwrap()
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &input).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.sources["org"].key, "test-secret");
    assert_eq!(loaded.bind, "0.0.0.0:8080");
    assert_eq!(loaded.routes[0].tag_prefix, None);
    assert!(loaded.routes[0].exclude_tags.is_empty());

    for patterns in ["[]", r#"["^web-derive-.*$", "-(internal|test)$"]"#] {
        std::fs::write(&path, format!("{input}\nexclude_tags = {patterns}\n")).unwrap();
        Config::load(&path).unwrap();
    }

    for invalid in [
        input.replace("destinations = [\"web\"]", "destinations = [\"missing\"]"),
        input.replace("name = \"web\"", "name = \"\""),
        input.replace(
            "repository = \"actix/actix-web\"",
            "repository = \"other/repo\"",
        ),
        format!("unknown = \"private-token\"\n{input}"),
        format!("{input}\nexclude_tags = [\"web-[\"]\n"),
        format!("{input}\nexclude_tags = [\"private-token(\"]\n"),
    ] {
        std::fs::write(&path, invalid).unwrap();
        let error = Config::load(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("private-token"));
    }
    std::fs::write(&path, &input).unwrap();
    for url in [
        "http://discord.com/api/webhooks/123/private-token",
        "https://other.example/api/webhooks/123/private-token",
        "https://discord.com/api/webhooks/123/private-token/github",
        "https://discord.com/api/webhooks/123/private-token?wait=false",
    ] {
        std::fs::write(&hook, url).unwrap();
        let error = Config::load(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("private-token"));
    }
    std::fs::write(
        &hook,
        "https://discord.com/api/v10/webhooks/123/private-token",
    )
    .unwrap();
    Config::load(&path).unwrap();
}

#[actix_web::test]
async fn worker_retries_fake_discord_and_finishes_active_send_on_shutdown() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release_send = Arc::new(tokio::sync::Notify::new());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let fake_count = count.clone();
    let fake_entered = entered.clone();
    let fake_release = release_send.clone();
    let server = HttpServer::new(move || {
        let count = fake_count.clone();
        let entered = fake_entered.clone();
        let release = fake_release.clone();
        App::new().default_service(web::post().to(move || {
            let count = count.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                match count.fetch_add(1, Ordering::SeqCst) {
                    0 => HttpResponse::TooManyRequests().json(json!({"retry_after": 0})),
                    1 => HttpResponse::InternalServerError().finish(),
                    _ => {
                        entered.notify_one();
                        release.notified().await;
                        HttpResponse::Ok().json(json!({"id": "confirmed"}))
                    }
                }
            }
        }))
    })
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let queue = Queue::default();
    let mut config = config();
    config.destinations.get_mut("web").unwrap().url = format!("http://{address}/webhook");
    let config = Arc::new(config);
    let release = serde_json::from_value(event()["release"].clone()).unwrap();
    queue
        .enqueue(
            "worker-test",
            "actix/actix-web",
            123,
            &routing::destinations(&config, "actix/actix-web", &release),
            &discord::render("actix/actix-web", &release),
        )
        .await
        .unwrap();
    let health = worker::WorkerHealth(Arc::new(AtomicBool::new(false)));
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(worker::run(
        queue.clone(),
        config.clone(),
        receiver,
        health.clone(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(15), entered.notified())
        .await
        .unwrap();
    stop.send_replace(true);
    assert!(!task.is_finished());
    release_send.notify_one();
    task.await.unwrap().unwrap();
    assert!(queue.claim().await.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(!health.0.load(Ordering::Relaxed));
    handle.stop(true).await;
}

#[actix_web::test]
async fn chunked_size_limit_and_ambiguous_signature_headers_are_rejected() {
    let body = br#"{ "x": 1 }"#;
    for (headers, status) in [
        (
            vec![
                ("x-hub-signature-256", sign(body)),
                ("x-hub-signature-256", sign(body)),
            ],
            401,
        ),
        (
            vec![
                ("x-hub-signature-256", sign(body)),
                ("content-encoding", "gzip".into()),
            ],
            415,
        ),
    ] {
        let mut builder = test::TestRequest::post()
            .param("source", "org")
            .insert_header(("content-type", "application/json"))
            .app_data(Data::new(config()));
        for header in headers {
            builder = builder.append_header(header);
        }
        let (request, mut payload) = builder.set_payload(body.as_slice()).to_http_parts();
        let error = github::SignedBody::from_request(&request, &mut payload)
            .await
            .err()
            .unwrap();
        assert_eq!(
            actix_web::Error::from(error)
                .as_response_error()
                .status_code(),
            status
        );
    }

    let request = test::TestRequest::post()
        .param("source", "org")
        .insert_header(("content-type", "application/json"))
        .insert_header(("x-hub-signature-256", sign(body)))
        .app_data(Data::new(config()))
        .app_data(web::JsonConfig::default().limit(8))
        .to_http_request();
    let chunks = vec![
        Ok(Bytes::from_static(b"{ \"x\":")),
        Ok(Bytes::from_static(b" 1 }")),
    ];
    let stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, actix_web::error::PayloadError>>>,
    > = Box::pin(futures_util::stream::iter(chunks));
    let mut payload = Payload::from(stream);
    let error = github::SignedBody::from_request(&request, &mut payload)
        .await
        .err()
        .unwrap();
    assert_eq!(
        actix_web::Error::from(error)
            .as_response_error()
            .status_code(),
        413
    );
}

#[tokio::test(start_paused = true)]
async fn queue_deduplicates_retries_and_resets_on_restart() {
    let queue = Queue::default();
    let release = serde_json::from_value(event()["release"].clone()).unwrap();
    let message = discord::render("actix/actix-web", &release);
    let destinations = routing::destinations(&config(), "actix/actix-web", &release);
    assert_eq!(
        queue
            .enqueue("first", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        1
    );
    assert!(
        !queue
            .enqueue("first", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .delivery_inserted
    );
    let job = queue.claim().await.unwrap();
    assert_eq!(job.attempts, 1);
    assert!(queue.claim().await.is_none());
    assert_eq!(
        queue
            .enqueue("second", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        0
    );
    queue.finish(job, true, 10, false, true).await;
    queue
        .enqueue("other", "actix/actix-web", 124, &destinations, &message)
        .await
        .unwrap();
    assert!(queue.claim().await.is_none());
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    let other = queue.claim().await.unwrap();
    assert_eq!(other.delivery, "other");
    queue.finish(other, false, 0, true, false).await;
    let retry = queue.claim().await.unwrap();
    assert_eq!(retry.attempts, 2);
    queue.finish(retry, false, 0, true, false).await;
    assert_eq!(
        queue
            .enqueue("third", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        0
    );

    let restarted = Queue::default();
    assert!(restarted.claim().await.is_none());
    assert_eq!(
        restarted
            .enqueue("first", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        1
    );

    tokio::time::advance(std::time::Duration::from_secs(30 * 24 * 60 * 60)).await;
    assert_eq!(
        queue
            .enqueue("first", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        1
    );
}

#[actix_web::test]
async fn full_queue_rejects_atomically_and_failed_jobs_allow_redelivery() {
    let queue = Queue::default();
    let release = serde_json::from_value(event()["release"].clone()).unwrap();
    let message = discord::render("actix/actix-web", &release);
    let destinations = routing::destinations(&config(), "actix/actix-web", &release);
    for id in 0..63 {
        queue
            .enqueue(
                &format!("fill-{id}"),
                "actix/actix-web",
                id,
                &destinations,
                &message,
            )
            .await
            .unwrap();
    }
    let two = BTreeMap::from([("web".into(), vec![]), ("other".into(), vec![])]);
    let error = queue
        .enqueue("overflow", "actix/actix-web", 100, &two, &message)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "delivery queue is full");
    let job = queue.claim().await.unwrap();
    // An in-flight job still consumes capacity.
    assert!(
        queue
            .enqueue("overflow", "actix/actix-web", 100, &two, &message)
            .await
            .is_err()
    );
    queue.finish(job, false, 0, false, false).await;
    assert_eq!(
        queue
            .enqueue("overflow", "actix/actix-web", 100, &two, &message)
            .await
            .unwrap()
            .jobs_inserted,
        2
    );
    let job = queue.claim().await.unwrap();
    let delivery = job.delivery.clone();
    queue.finish(job, false, 0, false, false).await;
    assert_eq!(
        queue
            .enqueue(&delivery, "actix/actix-web", 1, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        1
    );
}

#[actix_web::test]
async fn full_queue_returns_503_and_accepts_the_same_delivery_after_capacity_frees() {
    let queue = Queue::default();
    let release = serde_json::from_value(event()["release"].clone()).unwrap();
    let message = discord::render("actix/actix-web", &release);
    let destinations = routing::destinations(&config(), "actix/actix-web", &release);
    for id in 0..64 {
        queue
            .enqueue(
                &format!("fill-{id}"),
                "actix/actix-web",
                id,
                &destinations,
                &message,
            )
            .await
            .unwrap();
    }
    let app = test::init_service(
        App::new()
            .app_data(Data::new(config()))
            .app_data(Data::new(queue.clone()))
            .configure(github::endpoints),
    )
    .await;

    for expected in [503, 202] {
        let body = event().to_string();
        let request = test::TestRequest::post()
            .uri("/webhooks/github/org")
            .insert_header(("content-type", "application/json"))
            .insert_header(("x-hub-signature-256", sign(body.as_bytes())))
            .insert_header(("x-github-delivery", "overflow"))
            .insert_header(("x-github-event", "release"))
            .set_payload(body)
            .to_request();
        assert_eq!(test::call_service(&app, request).await.status(), expected);
        if expected == 503 {
            let job = queue.claim().await.unwrap();
            queue.finish(job, false, 0, true, false).await;
        }
    }
    let mut accepted = 0;
    while let Some(job) = queue.claim().await {
        if job.delivery == "overflow" {
            accepted += 1;
        }
        queue.finish(job, false, 0, true, false).await;
    }
    assert_eq!(accepted, 1);
}

#[actix_web::test]
async fn redelivery_retries_only_failed_destinations() {
    let queue = Queue::default();
    let release = serde_json::from_value(event()["release"].clone()).unwrap();
    let message = discord::render("actix/actix-web", &release);
    let destinations = BTreeMap::from([("a".into(), vec![]), ("b".into(), vec![])]);
    let (first, duplicate) = tokio::join!(
        queue.enqueue("delivery", "actix/actix-web", 123, &destinations, &message),
        queue.enqueue("delivery", "actix/actix-web", 123, &destinations, &message),
    );
    assert_eq!(
        first.unwrap().jobs_inserted + duplicate.unwrap().jobs_inserted,
        2
    );
    let sent = queue.claim().await.unwrap();
    assert_eq!(sent.destination, "a");
    queue.finish(sent, false, 0, true, false).await;
    let failed = queue.claim().await.unwrap();
    queue.finish(failed, false, 0, false, false).await;
    assert_eq!(
        queue
            .enqueue("delivery", "actix/actix-web", 123, &destinations, &message)
            .await
            .unwrap()
            .jobs_inserted,
        1
    );
    assert_eq!(queue.claim().await.unwrap().destination, "b");
    assert!(queue.claim().await.is_none());
}
