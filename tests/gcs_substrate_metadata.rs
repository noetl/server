//! `GcsSubstrate` sizes and existence-checks objects WITHOUT downloading them —
//! [noetl/ai-meta#479](https://github.com/noetl/ai-meta/issues/479).
//!
//! Run against an in-process stub of the GCS JSON API rather than the emulator, so CI runs it:
//! the property is about which REQUESTS are made, and a stub can count them where an
//! emulator cannot. The real reply shape (`size` as a string) is checked against
//! fake-gcs-server in `gcs_substrate_roundtrip.rs`.
//!
//! ⚠ The media counter has a positive control (`get_all` must move it). Without one, a stub
//! that never recognised a download would report zero downloads for every implementation,
//! and these tests would pass against the code they exist to reject.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use ehdb_core::EhdbError;
use ehdb_l0::DurableSubstrate;
use noetl_server::services::gcs_substrate::GcsSubstrate;

#[derive(Default)]
struct Counts {
    media: AtomicUsize,
    metadata: AtomicUsize,
}

/// Object name -> metadata reply body (None = serve the bytes' length as a string).
struct Stub {
    objects: HashMap<String, (Vec<u8>, Option<String>)>,
    counts: Counts,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

async fn serve(stub: Arc<Stub>, req: Request<Body>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let Some(rest) = path.strip_prefix("/storage/v1/b/bucket/o/") else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };
    let key = pct_decode(rest);
    let media = query.split('&').any(|kv| kv == "alt=media");
    if media {
        stub.counts.media.fetch_add(1, Ordering::SeqCst);
    } else {
        stub.counts.metadata.fetch_add(1, Ordering::SeqCst);
    }
    let Some((bytes, meta)) = stub.objects.get(&key) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };
    let body = if media {
        Body::from(bytes.clone())
    } else {
        Body::from(
            meta.clone()
                .unwrap_or_else(|| format!("{{\"size\":\"{}\"}}", bytes.len())),
        )
    };
    Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap()
}

/// Start the stub on its own thread and runtime: the substrate blocks on ITS runtime, so the
/// server must not share the test thread.
fn start(objects: HashMap<String, (Vec<u8>, Option<String>)>) -> (String, Arc<Stub>) {
    let stub = Arc::new(Stub {
        objects,
        counts: Counts::default(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let s = stub.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let app = axum::Router::new().fallback(move |req: Request<Body>| serve(s.clone(), req));
            axum::serve(listener, app).await.unwrap();
        });
    });
    let addr = rx.recv().unwrap();
    (format!("http://{addr}"), stub)
}

fn fixture() -> (GcsSubstrate, Arc<Stub>) {
    let mut objects = HashMap::new();
    objects.insert("ehdb/parts/a".to_string(), (b"hello world".to_vec(), None));
    objects.insert("ehdb/parts/empty".to_string(), (Vec::new(), None));
    objects.insert(
        "ehdb/parts/numeric".to_string(),
        (b"abc".to_vec(), Some("{\"size\":3}".to_string())),
    );
    objects.insert(
        "ehdb/parts/nosize".to_string(),
        (b"abc".to_vec(), Some("{\"name\":\"x\"}".to_string())),
    );
    let (endpoint, stub) = start(objects);
    let s = GcsSubstrate::open(&endpoint, "bucket", "ehdb").expect("construct");
    (s, stub)
}

fn media(stub: &Stub) -> usize {
    stub.counts.media.load(Ordering::SeqCst)
}

#[test]
fn object_size_reads_metadata_and_downloads_nothing() {
    let (s, stub) = fixture();
    assert_eq!(s.object_size("parts/a").unwrap(), 11);
    assert_eq!(
        s.object_size("parts/empty").unwrap(),
        0,
        "a zero-length object is 0, not absent"
    );
    assert_eq!(
        s.object_size("parts/numeric").unwrap(),
        3,
        "a numeric size is accepted too"
    );
    assert_eq!(
        media(&stub),
        0,
        "object_size DOWNLOADED the object to take its length — the trait default, \
         which this override exists to replace"
    );
    assert!(stub.counts.metadata.load(Ordering::SeqCst) >= 3);

    // Positive control: the stub does see a download when one happens.
    assert_eq!(s.get_all("parts/a").unwrap(), b"hello world");
    assert_eq!(
        media(&stub),
        1,
        "the media counter is blind; this test measures nothing"
    );
}

#[test]
fn exists_reads_metadata_and_downloads_nothing() {
    let (s, stub) = fixture();
    assert!(s.exists("parts/a").unwrap());
    assert!(s.exists("parts/empty").unwrap(), "an empty object exists");
    assert!(!s.exists("parts/missing").unwrap());
    assert_eq!(
        media(&stub),
        0,
        "exists DOWNLOADED the object to answer a boolean (noetl/ai-meta#479)"
    );
}

#[test]
fn an_absent_or_unsized_object_is_an_error_not_zero() {
    let (s, _stub) = fixture();
    match s.object_size("parts/missing") {
        Err(EhdbError::NotFound(k)) => assert!(k.ends_with("parts/missing"), "{k}"),
        other => panic!("an absent object must be NotFound, got {other:?}"),
    }
    assert!(
        s.object_size("parts/nosize").is_err(),
        "a metadata reply with no size must not read as a 0-byte object"
    );
}
