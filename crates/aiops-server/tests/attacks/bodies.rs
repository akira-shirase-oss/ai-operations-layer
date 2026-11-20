//! Body attacks: over the 1 MiB cap, past the 500-event bound, truncated and
//! compressed tricks, and a client that sends a byte a heartbeat — the server
//! must stay bounded and stay up.

use crate::common::{
    batch, client, create_ingest_key, create_project, create_token, get, journey, post_ingest,
    stack,
};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn bodies_over_every_cap_are_shed_not_parsed(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    // Declared 2 MiB → refused; the collector answers 413 early and may cut
    // the connection mid-write (a body shed before it is read). Either is the
    // refusal — a broken write pipe is the client seeing the shed.
    let huge = "x".repeat(2 * 1024 * 1024);
    let r = client()
        .post(format!("{}/v1/ingest", s.base))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body(huge)
        .send()
        .await;
    match r {
        Ok(res) => {
            assert_eq!(res.status().as_u16(), 413, "{}", res.status());
            let body: serde_json::Value = res.json().await.unwrap_or_default();
            assert_eq!(body["error"]["code"], "payload_too_large");
        }
        Err(e) => assert!(e.is_request(), "only the write may fail: {e}"),
    }

    // 501 events in a small body → the batch bound, not the byte bound.
    let one = serde_json::json!({
        "event_id": "00000000000000000000000000000001",
        "type": "gap.reported", "operation_id": "0a".repeat(16),
        "seq": 1, "op_seq": 1, "event_time": "2026-11-20T00:00:00Z",
        "gap": {"kind": "collector_downtime", "missing_seqs": []}
    });
    let many = serde_json::json!({
        "schema_version": 1,
        "source": {"instance_id": "ab".repeat(16), "service": "h", "sdk": "t"},
        "events": vec![one; 501]
    });
    let (status, body, _) = post_ingest(&s.base, Some(&key), many.to_string()).await;
    assert!(
        matches!(status, 400 | 413),
        "over the event bound: {status} {body}"
    );

    // Truncated JSON mid-event and NUL bytes — parse errors, never a hang or
    // a 500.
    for body in [
        "{\"schema_version\":1,\"events\":[{\"event_id\":\"01".to_owned(),
        "\0\0\0".to_owned(),
    ] {
        let (status, b, _) = post_ingest(&s.base, Some(&key), body).await;
        assert!(
            (400..500).contains(&status),
            "malformed body answered {status}: {b}"
        );
    }
    // Genuinely gzip-compressed bytes: never decompressed, never expanded.
    let gzipped = gzip(&batch(&journey(1, 0)));
    let r = client()
        .post(format!("{}/v1/ingest", s.base))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .body(gzipped)
        .send()
        .await
        .unwrap();
    assert!(
        (400..500).contains(&r.status().as_u16()),
        "a compressed body is not silently expanded: {}",
        r.status()
    );
}

/// A valid gzip envelope (stored blocks) around the payload — the point is a
/// compressed body the collector must not expand.
fn gzip(data: &str) -> Vec<u8> {
    let data = data.as_bytes();
    let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    let mut i = 0;
    while i < data.len() {
        let n = (data.len() - i).min(0xffff);
        let last = i + n == data.len();
        out.push(u8::from(last));
        out.extend_from_slice(&u16::try_from(n).unwrap().to_le_bytes());
        out.extend_from_slice(&(!u16::try_from(n).unwrap()).to_le_bytes());
        out.extend_from_slice(&data[i..i + n]);
        i += n;
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xffff_ffff;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                (c >> 1) ^ 0xedb8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_body_sent_one_byte_a_heartbeat_is_bounded(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);

    // A raw TCP client writes request headers, then a single byte every 150 ms
    // under a false content-length — then goes silent. The server must not hold
    // the connection open forever, and must keep serving everyone else.
    let addr = s.base.strip_prefix("http://").unwrap().to_owned();
    let trickler = tokio::spawn(async move {
        let Ok(mut conn) = tokio::net::TcpStream::connect(&addr).await else {
            return;
        };
        let body = batch(&journey(1, 0));
        let _ = conn
            .write_all(
                format!(
                    "POST /v1/ingest HTTP/1.1\r\nHost: {addr}\r\n\
                     authorization: Bearer {key}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\n\r\n",
                    body.len() + 9000
                )
                .as_bytes(),
            )
            .await;
        for b in body.bytes().take(12) {
            let _ = conn.write_all(&[b]).await;
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let mut buf = [0u8; 512];
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), conn.read(&mut buf)).await;
    });

    // While the trickler holds its connection, the server still answers.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&reader)).await;
    assert_eq!(status, 200, "a trickler must not stall the collector");
    trickler.await.unwrap();
}
