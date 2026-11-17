//! A1's contract failure cases, end to end: the real `reference-host` binary, the
//! collector on a real TCP port (in process, against real PostgreSQL), and the query API
//! a console uses. Needs `DATABASE_URL`.
//!
//! - the full journey, each fault class told apart (transport, validation, rejected,
//!   uncertain external effect);
//! - collector unavailable: the host keeps working; drops and gaps are visible afterwards;
//! - duplicate and out-of-order delivery (a chaos proxy): merged, no false chronology;
//! - a host flooding events: bounded and counted, the pilot unaffected;
//! - a console user without scope or project: denied and audited.
#![expect(
    clippy::unwrap_used,
    reason = "end-to-end tests: a failed setup step is a test failure"
)]

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aiops_server::auth::{INGEST_KEY_PREFIX, TOKEN_PREFIX, generate, hash};
use aiops_server::{AppState, Config, router, system_clock};
use aiops_store::Store;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

/// A fresh, migrated database on a multi-threaded runtime (`sqlx::test`'s single thread
/// would let load-generating tasks starve the collector under test). Leftovers of failed
/// runs older than an hour are dropped by the next one.
struct ScratchDb {
    pool: PgPool,
    admin: PgPool,
    name: String,
}

impl ScratchDb {
    async fn new() -> Self {
        let admin_url = std::env::var("DATABASE_URL").unwrap();
        let admin = PgPool::connect(&admin_url).await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale: Vec<String> =
            sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE 'aiops_mt_%'")
                .fetch_all(&admin)
                .await
                .unwrap();
        for name in stale {
            let born = name
                .split('_')
                .nth(2)
                .and_then(|t| t.parse::<u64>().ok())
                .unwrap_or(0);
            if now.saturating_sub(born) > 3_600 {
                let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                )))
                .execute(&admin)
                .await;
            }
        }
        let name = format!(
            "aiops_mt_{now}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let (prefix, _) = admin_url.rsplit_once('/').unwrap();
        let pool = PgPool::connect(&format!("{prefix}/{name}")).await.unwrap();
        Store::from_pool(pool.clone()).migrate().await.unwrap();
        Self { pool, admin, name }
    }

    async fn drop(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

struct Collector {
    base: String,
    store: Store,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn collector_on(
    pool: &PgPool,
    addr: SocketAddr,
    tune: impl FnOnce(&mut Config),
) -> Collector {
    // The collector as deployed: separate ingest, query, control and auth pools (one shared
    // pool lets one surface's load time out another's, which production never does).
    let base = (*pool.connect_options()).clone();
    let store = Store::connect_with(&aiops_store::StoreConfig::new(""), &base)
        .await
        .unwrap();
    let mut config = Config::local("");
    tune(&mut config);
    let mut state = AppState::new(store.clone(), config.clone(), system_clock(), None);
    state.ingest_buckets =
        aiops_server::auth::Buckets::new(config.ingest_events_per_sec, config.ingest_burst_events);
    let app = router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Collector {
        base: format!("http://{addr}"),
        store,
        server,
    }
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

struct Keys {
    ingest: String,
    flood: String,
    reader: String,
    no_read: String,
    other_reader: String,
    admin: String,
}

async fn keys(store: &Store) -> Keys {
    store.create_project("pilot").await.unwrap();
    store.create_project("other").await.unwrap();
    let ingest = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key("pilot", "reference-host", &hash(&ingest))
        .await
        .unwrap();
    let flood = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key("pilot", "noisy-host", &hash(&flood))
        .await
        .unwrap();
    let later = system_clock()().plus_micros(3_600_000_000);
    let token = async |actor: &str, scope: &str, project: &str| {
        let t = generate(TOKEN_PREFIX).unwrap();
        store
            .create_token(
                actor,
                &[scope.to_owned()],
                &[project.to_owned()],
                &hash(&t),
                later,
            )
            .await
            .unwrap();
        t
    };
    Keys {
        reader: token("ana", "read", "pilot").await,
        no_read: token("bob", "operate", "pilot").await,
        other_reader: token("eve", "read", "other").await,
        admin: token("ops", "admin", "pilot").await,
        ingest,
        flood,
    }
}

struct Host {
    child: Child,
    base: String,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn start_host(collector: &str, key: &str) -> Host {
    // A port released by free_addr() can be taken by another socket before the child
    // binds it (parallel tests, a loaded machine): if the child exits, try a fresh port.
    for _ in 0..5 {
        let addr = free_addr();
        let mut child = Command::new(env!("CARGO_BIN_EXE_reference-host"))
            .env("REFHOST_LISTEN", addr.to_string())
            .env("AIOPS_ENDPOINT", collector)
            .env("AIOPS_INGEST_KEY", key)
            .env("REFHOST_ALLOW_FAULTS", "1")
            .env("REFHOST_MODEL_LATENCY_MS", "4")
            .env("REFHOST_TIMEOUT_MS", "40")
            .env("REFHOST_LOG_FILE", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client.get(format!("{base}/healthz")).send().await.is_ok() {
                return Host { child, base };
            }
            if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("reference host did not start on any of five ports");
}

async fn article(host: &Host, request_id: &str, fault: Option<&str>) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{}/articles", host.base))
        .header("x-request-id", request_id)
        .json(&json!({"topic": "durable queues", "fault": fault}))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn get_json(base: &str, path: &str, token: &str) -> (u16, Value) {
    let r = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn eventually<F: AsyncFnMut() -> bool>(what: &str, mut ok: F) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ok().await {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The timeline of the operation that served `request_id`, once its finish has arrived. On
/// a timeout the panic shows the last list answer, so a missing finish can be told from a
/// slow one.
async fn timeline(c: &Collector, token: &str, request_id: &str) -> Value {
    let mut found = Value::Null;
    let mut previous = Value::Null;
    eventually(
        &format!("timeline for {request_id} (see the last list answer above)"),
        async || {
            let (_, list) = get_json(
                &c.base,
                &format!("/v1/projects/pilot/operations?request_id={request_id}"),
                token,
            )
            .await;
            if list != previous {
                eprintln!("list for {request_id}: {list}");
                previous = list.clone();
            }
            let Some(op) = list["operations"].get(0) else {
                return false;
            };
            if op["status"]["state"] == "no_finish_observed" {
                return false;
            }
            let id = op["operation_id"].as_str().unwrap().to_owned();
            let (status, t) = get_json(
                &c.base,
                &format!("/v1/projects/pilot/operations/{id}"),
                token,
            )
            .await;
            found = t;
            status == 200
        },
    )
    .await;
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_journey_tells_transport_validation_rejection_and_uncertain_effects_apart() {
    let db = ScratchDb::new().await;
    let pool = db.pool.clone();
    let c = collector_on(&pool, free_addr(), |_| {}).await;
    let k = keys(&c.store).await;
    let host = start_host(&c.base, &k.ingest).await;

    let cases = [
        ("ok-1", None, 201),
        ("retry-1", Some("model_transport_once"), 201),
        ("invalid-1", Some("invalid_draft"), 422),
        ("rejected-1", Some("publish_rejected"), 502),
        ("lost-1", Some("publish_lost_response"), 504),
        ("tool-1", Some("tool_timeout"), 504),
        ("exhausted-1", Some("model_timeout_all"), 502),
    ];
    for (rid, fault, want) in cases {
        let (status, _) = article(&host, rid, fault).await;
        assert_eq!(status, want, "{rid}");
    }

    let t = timeline(&c, &k.reader, "ok-1").await;
    assert_eq!(t["status"]["state"], "succeeded");
    assert_eq!(t["coverage"]["missing_known"], 0);
    assert_eq!(t["coverage"]["missing_unknown"], false);
    let names: Vec<&str> = t["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["generate", "validate", "fact_check", "publish"]);
    assert_eq!(t["cost"]["label"], "estimate");
    assert!(t["cost"]["estimate_micros"].as_u64().unwrap() > 0);

    let t = timeline(&c, &k.reader, "retry-1").await;
    assert_eq!(t["status"]["state"], "succeeded");
    assert_eq!(t["flags"]["retried_steps"], 1);
    assert_eq!(t["steps"][0]["attempts"][0]["class"], "transport");
    assert_eq!(t["steps"][0]["attempts"][1]["state"], "succeeded");

    let t = timeline(&c, &k.reader, "invalid-1").await;
    assert_eq!(
        (t["status"]["state"].as_str(), t["status"]["class"].as_str()),
        (Some("failed"), Some("validation"))
    );
    assert_eq!(
        t["steps"].as_array().unwrap().len(),
        2,
        "nothing after validation ran"
    );

    let t = timeline(&c, &k.reader, "rejected-1").await;
    assert_eq!(
        (t["status"]["state"].as_str(), t["status"]["class"].as_str()),
        (Some("failed"), Some("rejected"))
    );
    assert_eq!(
        t["flags"]["external_effect_unknown"], false,
        "a definitive refusal is not uncertain"
    );

    let t = timeline(&c, &k.reader, "lost-1").await;
    assert_eq!(t["status"]["state"], "uncertain");
    assert_eq!(t["status"]["reason"], "timeout_after_send");
    assert_eq!(t["flags"]["external_effect_unknown"], true);
    assert_eq!(t["steps"][3]["attempts"][0]["state"], "uncertain");
    // The trace says "unknown"; the authority knows: it was published, exactly once.
    let (status, p) = get_json(&host.base, "/publications/article:lost-1", "").await;
    assert_eq!((status, p["times_published"].as_u64()), (200, Some(1)));
    let (status, _) = get_json(&host.base, "/publications/article:rejected-1", "").await;
    assert_eq!(status, 404);

    let t = timeline(&c, &k.reader, "tool-1").await;
    assert_eq!(t["status"]["state"], "failed");
    assert_eq!(t["steps"][2]["attempts"][0]["state"], "uncertain");
    assert_eq!(
        t["flags"]["external_effect_unknown"], false,
        "the fact check has no external effect"
    );

    let t = timeline(&c, &k.reader, "exhausted-1").await;
    assert_eq!(t["status"]["state"], "failed");
    assert_eq!(t["steps"][0]["attempts"].as_array().unwrap().len(), 3);
    assert!(
        t["cost"]["estimate_micros"].as_u64().unwrap() > 0,
        "timed-out model calls still cost (estimate)"
    );
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_collector_down_the_host_keeps_working_and_the_gaps_show_afterwards() {
    let db = ScratchDb::new().await;
    let pool = db.pool.clone();
    let addr = free_addr(); // nothing listens here yet
    let base = format!("http://{addr}");
    let store = Store::from_pool(pool.clone());
    let k = keys(&store).await;
    let host = start_host(&base, &k.ingest).await;

    let started = Instant::now();
    for i in 0..20 {
        let (status, _) = article(&host, &format!("down-{i}"), None).await;
        assert_eq!(
            status, 201,
            "the host serves requests while the collector is down"
        );
    }
    let host_time = started.elapsed();
    assert!(
        host_time < Duration::from_secs(5),
        "20 requests took {host_time:?}"
    );
    let stats_url = format!("{}/aiops/stats", host.base);
    let stats = async || -> Value {
        reqwest::get(&stats_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    eventually("the SDK gives up on the dead collector", async || {
        stats().await["dropped"] == 200
    })
    .await;

    // The collector comes back on the same address.
    let c = collector_on(&pool, addr, |_| {}).await;
    for i in 0..5 {
        article(&host, &format!("up-{i}"), None).await;
    }
    // An operation whose events straddled two failed batches gets two reports, so count
    // events, not reports: every dropped event must reach the collector as a gap.
    eventually("gap reports delivered", async || {
        let s = stats().await;
        s["gap_reports_sent"].as_u64().unwrap() >= 20 && s["batches_sent"].as_u64().unwrap() >= 1
    })
    .await;

    let t = timeline(&c, &k.reader, "up-0").await;
    assert_eq!(t["status"]["state"], "succeeded");
    assert_eq!(t["coverage"]["missing_known"], 0);

    let (_, list) = get_json(
        &c.base,
        "/v1/projects/pilot/operations?limit=200",
        &k.reader,
    )
    .await;
    let ops = list["operations"].as_array().unwrap();
    assert_eq!(
        ops.len(),
        25,
        "the 20 lost operations are known through their gap reports"
    );
    let lost_ops: Vec<&Value> = ops
        .iter()
        .filter(|o| o["status"]["state"] == "no_finish_observed")
        .collect();
    assert_eq!(lost_ops.len(), 20);
    let reported: i64 = lost_ops
        .iter()
        .map(|o| o["reported_dropped"].as_i64().unwrap())
        .sum();
    assert_eq!(
        reported, 200,
        "every dropped event is accounted for, by the host and the collector"
    );
    for op in &lost_ops {
        let id = op["operation_id"].as_str().unwrap();
        let (_, t) = get_json(
            &c.base,
            &format!("/v1/projects/pilot/operations/{id}"),
            &k.reader,
        )
        .await;
        assert_eq!(
            t["status"]["state"], "no_finish_observed",
            "never \"did not run\""
        );
        assert_eq!(t["flags"]["start_not_observed"], true);
        assert_eq!(t["coverage"]["missing_unknown"], true);
        let gaps = t["gaps"].as_array().unwrap();
        assert!(
            gaps.iter()
                .all(|g| g["kind"] == "sdk_reported" && g["reason"] == "send_failed")
        );
        let dropped: u64 = gaps.iter().map(|g| g["count"].as_u64().unwrap()).sum();
        assert_eq!(
            dropped, 10,
            "all ten events of the operation are reported lost"
        );
    }
    db.drop().await;
}

/// Forwards every batch twice and delivers every other batch late, after the next one.
#[derive(Clone)]
struct Chaos {
    upstream: String,
    held: Arc<Mutex<Option<(HeaderMap, Bytes)>>>,
    http: reqwest::Client,
}

async fn chaos_ingest(
    State(chaos): State<Chaos>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let send = async |h: &HeaderMap, b: Bytes| {
        let r = chaos
            .http
            .post(format!("{}/v1/ingest", chaos.upstream))
            .header("authorization", h.get("authorization").unwrap())
            .header("content-type", "application/json")
            .body(b)
            .send()
            .await
            .unwrap();
        (
            StatusCode::from_u16(r.status().as_u16()).unwrap(),
            r.text().await.unwrap(),
        )
    };
    let late = chaos.held.lock().unwrap().take();
    if late.is_none() {
        // Hold this batch; acknowledge it as if it had been delivered (it will be, later).
        *chaos.held.lock().unwrap() = Some((headers, body.clone()));
        return (StatusCode::OK, json!({"accepted": 0, "duplicates": 0, "refused": [], "withheld_attributes": 0, "redactions": 0}).to_string());
    }
    let first = send(&headers, body.clone()).await;
    send(&headers, body).await; // duplicate
    if let Some((h, b)) = late {
        send(&h, b.clone()).await; // out of order
        send(&h, b).await; // and duplicated
    }
    first
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicated_and_reordered_batches_merge_without_false_chronology() {
    let db = ScratchDb::new().await;
    let pool = db.pool.clone();
    let c = collector_on(&pool, free_addr(), |_| {}).await;
    let k = keys(&c.store).await;
    let chaos = Chaos {
        upstream: c.base.clone(),
        held: Arc::new(Mutex::new(None)),
        http: reqwest::Client::new(),
    };
    let proxy_addr = free_addr();
    let app = axum::Router::new()
        .route("/v1/ingest", axum::routing::post(chaos_ingest))
        .with_state(chaos.clone());
    let listener = tokio::net::TcpListener::bind(proxy_addr).await.unwrap();
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await });
    let host = start_host(&format!("http://{proxy_addr}"), &k.ingest).await;

    for i in 0..12 {
        article(&host, &format!("chaos-{i}"), None).await;
        tokio::time::sleep(Duration::from_millis(60)).await; // several batches, not one
    }
    // Flush whatever the proxy still holds by sending one more operation.
    // The proxy holds every other batch until the next one arrives, so the last batch can
    // stay held forever (on a loaded machine the final articles share one batch). Once the
    // SDK has flushed, deliver whatever is still held, late and twice, like the rest.
    article(&host, "chaos-last", None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let held = chaos.held.lock().unwrap().take();
    if let Some((headers, body)) = held {
        for _ in 0..2 {
            chaos
                .http
                .post(format!("{}/v1/ingest", chaos.upstream))
                .header("authorization", headers.get("authorization").unwrap())
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .unwrap();
        }
    }

    for i in 0..12 {
        let t = timeline(&c, &k.reader, &format!("chaos-{i}")).await;
        assert_eq!(
            t["coverage"]["events_received"], 10,
            "chaos-{i}: each event once"
        );
        assert_eq!(t["coverage"]["missing_known"], 0);
        let entries = t["entries"].as_array().unwrap();
        let times: Vec<&str> = entries
            .iter()
            .map(|e| e["event_time"].as_str().unwrap())
            .collect();
        let mut sorted = times.clone();
        sorted.sort_unstable();
        assert_eq!(
            times, sorted,
            "ordered by event time, whatever the arrival order"
        );
        let seqs: Vec<u64> = entries
            .iter()
            .map(|e| e["op_seq"].as_u64().unwrap())
            .collect();
        assert_eq!(
            seqs,
            (1..=10).collect::<Vec<_>>(),
            "one host: event time agrees with emission order"
        );
    }
    let (_, stats) = get_json(&c.base, "/v1/collector/stats", &k.admin).await;
    assert!(
        stats["events_duplicate"].as_u64().unwrap() > 0,
        "duplicates arrived and were merged: {stats}"
    );
    assert_eq!(stats["events_conflicting"], 0);
    proxy.abort();
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flooding_host_is_limited_and_counted_while_the_pilot_is_unaffected() {
    let db = ScratchDb::new().await;
    let pool = db.pool.clone();
    let c = collector_on(&pool, free_addr(), |cfg| {
        // Small enough that the second 500-event batch is over the limit at any machine
        // speed; the pilot's own key has the same bucket and needs ~150 events.
        cfg.ingest_events_per_sec = 200;
        cfg.ingest_burst_events = 600;
    })
    .await;
    let k = keys(&c.store).await;
    let host = start_host(&c.base, &k.ingest).await;

    let flood_base = c.base.clone();
    let flood_key = k.flood.clone();
    let flooder = tokio::spawn(async move {
        let http = reqwest::Client::new();
        let (mut ok, mut limited) = (0u32, 0u32);
        let started = Instant::now();
        let mut seq = 0u64;
        // Flood for at least 2 s and until the limiter has answered at least once (≤ 20 s).
        while started.elapsed() < Duration::from_secs(2)
            || (limited == 0 && started.elapsed() < Duration::from_secs(20))
        {
            let events: Vec<Value> = (0..500)
                .map(|_| {
                    seq += 1;
                    json!({"event_id": format!("{seq:032x}"), "type": "operation.started", "operation_id": format!("ff{seq:030x}"),
                           "seq": seq, "op_seq": 1, "event_time": aiops_domain::Timestamp::from_unix_micros(system_clock()().unix_micros()).to_rfc3339(), "name": "noise"})
                })
                .collect();
            let body = json!({"schema_version": 1, "source": {"instance_id": "99999999999999999999999999999999", "service": "noisy", "sdk": "flood/1"}, "events": events});
            let r = http
                .post(format!("{flood_base}/v1/ingest"))
                .bearer_auth(&flood_key)
                .json(&body)
                .send()
                .await
                .unwrap();
            match r.status().as_u16() {
                200 => ok += 1,
                429 => {
                    assert!(r.headers().contains_key("retry-after"));
                    limited += 1;
                }
                // Shed or store-busy under the flood: also a bounded, retryable answer.
                503 => {}
                s => panic!("unexpected status {s}"),
            }
        }
        (ok, limited)
    });

    for i in 0..15 {
        let (status, _) = article(&host, &format!("calm-{i}"), None).await;
        assert_eq!(status, 201);
    }
    let (ok, limited) = flooder.await.unwrap();
    assert!(
        limited > 0,
        "the flood was limited (ok {ok}, limited {limited})"
    );
    // Shown only if the test fails: what the pilot's SDK saw during the flood.
    let (_, sdk) = get_json(&host.base, "/aiops/stats", "").await;
    eprintln!("pilot SDK after the flood: {sdk} (flood: ok {ok}, limited {limited})");
    for i in 0..15 {
        let t = timeline(&c, &k.reader, &format!("calm-{i}")).await;
        assert_eq!(t["status"]["state"], "succeeded");
        assert_eq!(
            t["coverage"]["missing_known"], 0,
            "the pilot lost nothing to the flood"
        );
    }
    let (_, stats) = get_json(&c.base, "/v1/collector/stats", &k.admin).await;
    assert_eq!(
        stats["rate_limited"].as_u64(),
        Some(u64::from(limited)),
        "every refusal is counted"
    );
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_console_user_without_scope_or_project_is_denied_and_audited() {
    let db = ScratchDb::new().await;
    let pool = db.pool.clone();
    let c = collector_on(&pool, free_addr(), |_| {}).await;
    let k = keys(&c.store).await;
    let host = start_host(&c.base, &k.ingest).await;
    article(&host, "secret-1", None).await;
    let t = timeline(&c, &k.reader, "secret-1").await;
    let path = format!(
        "/v1/projects/pilot/operations/{}",
        t["operation_id"].as_str().unwrap()
    );

    let (status, body) = get_json(&c.base, &path, &k.no_read).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (403, Some("forbidden"))
    );
    let (status, _) = get_json(&c.base, &path, &k.other_reader).await;
    assert_eq!(status, 403);
    let (status, _) = get_json(&c.base, "/v1/projects/pilot/operations", &k.other_reader).await;
    assert_eq!(status, 403);
    let (status, _) = get_json(&c.base, &path, &k.ingest).await;
    assert_eq!(status, 401, "an ingest key cannot read");

    let audit = c.store.recent_audit(10).await.unwrap();
    let rows: Vec<(&str, &str, &str)> = audit
        .iter()
        .map(|a| (a.actor.as_str(), a.action.as_str(), a.reason.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("eve", "operations.list", "project_not_in_scope"),
            ("eve", "timeline.read", "project_not_in_scope"),
            ("bob", "timeline.read", "scope_missing"),
        ]
    );
    assert!(audit.iter().all(|a| a.decision == "denied"));
    db.drop().await;
}
