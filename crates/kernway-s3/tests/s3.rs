//! The S3 client against a live S3-compatible server — set up by
//! `tests/support/s3-setup.sh` (the Versity S3 Gateway), which prints
//! `KERNWAY_S3_*`. Without it each test reports `skip:`, which the CI job and
//! the acceptance run treat as a failure.

use kernway_http_client::{HttpClient, Method, Request, Url};
use kernway_s3::{PutOptions, S3Client, S3Config, S3Error};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

fn live() -> Option<S3Config> {
    let Ok(endpoint) = std::env::var("KERNWAY_S3_ENDPOINT") else {
        eprintln!("skip: KERNWAY_S3_ENDPOINT not set (tests/support/s3-setup.sh)");
        return None;
    };
    let v = |k: &str| std::env::var(k).unwrap();
    Some(S3Config::new(endpoint, v("KERNWAY_S3_ACCESS_KEY"), v("KERNWAY_S3_SECRET_KEY"), v("KERNWAY_S3_REGION")))
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn bucket(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("kwt-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

async fn empty_and_drop(s3: &S3Client, b: &str) {
    let _ = s3.delete_prefix(b, "").await;
    let _ = s3.delete_bucket(b).await;
}

#[test]
fn buckets_are_created_found_and_removed() {
    let Some(cfg) = live() else { return };
    block(async move {
        let s3 = S3Client::new(cfg).unwrap();
        let b = bucket("bkt");
        assert!(!s3.bucket_exists(&b).await.unwrap());
        assert!(s3.create_bucket(&b).await.unwrap(), "created");
        assert!(!s3.create_bucket(&b).await.unwrap(), "already yours");
        assert!(s3.bucket_exists(&b).await.unwrap());
        s3.put(&b, "x", b"x".to_vec(), &PutOptions::default()).await.unwrap();
        let e = s3.delete_bucket(&b).await.unwrap_err();
        assert!(matches!(&e, S3Error::Service { status: 409, code, .. } if code == "BucketNotEmpty"), "{e}");
        s3.delete(&b, "x").await.unwrap();
        s3.delete_bucket(&b).await.unwrap();
        assert!(!s3.bucket_exists(&b).await.unwrap());
        assert!(matches!(s3.put("Bad_Name", "k", b"".to_vec(), &PutOptions::default()).await, Err(S3Error::Config(_))));
    });
}

#[test]
fn objects_round_trip_byte_exact_with_metadata_and_ranges() {
    let Some(cfg) = live() else { return };
    block(async move {
        let s3 = S3Client::new(cfg).unwrap();
        let b = bucket("obj");
        s3.create_bucket(&b).await.unwrap();
        let every_byte: Vec<u8> = (0..=255u8).cycle().take(256 * 16).collect();
        let big: Vec<u8> = (0..3 << 20).map(|i| (i * 31 % 251) as u8).collect();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("bytes.bin", every_byte.clone()),
            ("empty", Vec::new()),
            ("big/3mib.bin", big.clone()),
            ("ảnh/khách hàng #1 (VIP).jpg", b"\xff\xd8\xff jpeg".to_vec()),
            ("a+b=c&d?e.txt", b"punctuation".to_vec()),
            ("deep/er/and/deeper/file.json", br#"{"ticket":42}"#.to_vec()),
        ];
        let opts = PutOptions {
            content_type: Some("image/jpeg".into()),
            metadata: vec![("salon".into(), "joy-nails".into()), ("ticket-id".into(), "42".into())],
            cache_control: Some("max-age=3600".into()),
        };
        for (k, body) in &cases {
            let etag = s3.put(&b, k, body.clone(), &opts).await.unwrap_or_else(|e| panic!("put {k}: {e}"));
            assert!(!etag.is_empty(), "{k}: an ETag");
            let o = s3.get(&b, k).await.unwrap().unwrap_or_else(|| panic!("get {k}"));
            assert_eq!(&o.body, body, "{k}: byte-exact");
            assert_eq!(o.size, body.len() as u64);
            assert_eq!(o.content_type.as_deref(), Some("image/jpeg"));
            assert!(o.metadata.contains(&("salon".into(), "joy-nails".into())), "{k}: {:?}", o.metadata);
            assert!(o.metadata.contains(&("ticket-id".into(), "42".into())));
        }
        // Ranges.
        let r = s3.get_range(&b, "big/3mib.bin", 1_000_000, 1_000_099).await.unwrap();
        assert_eq!(r.body, big[1_000_000..1_000_100]);
        assert_eq!(r.size, big.len() as u64, "the whole object's size");
        // Head, missing keys, delete.
        let h = s3.head(&b, "bytes.bin").await.unwrap().unwrap();
        assert_eq!((h.size, h.body.len()), (every_byte.len() as u64, 0));
        assert!(s3.get(&b, "no/such/key").await.unwrap().is_none());
        assert!(s3.head(&b, "no/such/key").await.unwrap().is_none());
        s3.delete(&b, "empty").await.unwrap();
        s3.delete(&b, "empty").await.unwrap();
        assert!(s3.get(&b, "empty").await.unwrap().is_none());
        // Overwrite.
        s3.put(&b, "bytes.bin", b"v2".to_vec(), &PutOptions::default()).await.unwrap();
        assert_eq!(s3.get(&b, "bytes.bin").await.unwrap().unwrap().body, b"v2");
        empty_and_drop(&s3, &b).await;
    });
}

#[test]
fn listing_pages_prefixes_and_copies() {
    let Some(cfg) = live() else { return };
    block(async move {
        let s3 = S3Client::new(cfg).unwrap();
        let b = bucket("list");
        s3.create_bucket(&b).await.unwrap();
        for i in 0..25 {
            let dir = ["avatars", "tickets", "receipts"][i % 3];
            s3.put(&b, &format!("{dir}/{i:03}.bin"), vec![i as u8; i + 1], &PutOptions::default()).await.unwrap();
        }
        // Pages of 10, following the tokens.
        let mut keys = Vec::new();
        let mut token = None;
        let mut pages = 0;
        loop {
            let p = s3.list(&b, "", None, 10, token.as_deref()).await.unwrap();
            assert!(p.objects.len() <= 10);
            keys.extend(p.objects.iter().map(|o| o.key.clone()));
            pages += 1;
            match p.next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        assert_eq!((keys.len(), pages), (25, 3));
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "keys come in order");
        // A prefix; "directories" with a delimiter.
        let tickets = s3.list_all(&b, "tickets/").await.unwrap();
        assert_eq!(tickets.len(), 8);
        assert!(tickets.iter().all(|o| o.key.starts_with("tickets/") && o.size > 0));
        let dirs = s3.list(&b, "", Some("/"), 100, None).await.unwrap();
        assert_eq!(dirs.common_prefixes, ["avatars/", "receipts/", "tickets/"]);
        assert!(dirs.objects.is_empty());
        // A server-side copy.
        s3.copy(&b, "avatars/000.bin", &b, "backup/avatar-0.bin").await.unwrap();
        assert_eq!(s3.get(&b, "backup/avatar-0.bin").await.unwrap().unwrap().body, vec![0u8; 1]);
        assert_eq!(s3.delete_prefix(&b, "avatars/").await.unwrap(), 9);
        assert!(s3.list_all(&b, "avatars/").await.unwrap().is_empty());
        empty_and_drop(&s3, &b).await;
    });
}

#[test]
fn multipart_uploads_complete_and_abort() {
    let Some(cfg) = live() else { return };
    block(async move {
        let s3 = S3Client::new(cfg).unwrap();
        let b = bucket("mp");
        s3.create_bucket(&b).await.unwrap();
        // 12 MiB in 5 MiB parts: three parts, the last short.
        let body: Vec<u8> = (0..12 << 20).map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let opts = PutOptions { content_type: Some("video/mp4".into()), ..PutOptions::default() };
        let etag = s3.put_multipart(&b, "videos/demo.mp4", &body, 5 << 20, &opts).await.unwrap();
        assert!(etag.ends_with("-3"), "a multipart ETag counts its parts: {etag}");
        let o = s3.get(&b, "videos/demo.mp4").await.unwrap().unwrap();
        assert_eq!(o.body.len(), body.len());
        assert!(o.body == body, "byte-exact");
        assert_eq!(o.content_type.as_deref(), Some("video/mp4"));
        // Abandoned: nothing is left under the key.
        let id = s3.multipart_start(&b, "videos/abandoned.mp4", &PutOptions::default()).await.unwrap();
        s3.multipart_part(&b, "videos/abandoned.mp4", &id, 1, vec![1u8; 5 << 20]).await.unwrap();
        s3.multipart_abort(&b, "videos/abandoned.mp4", &id).await.unwrap();
        assert!(s3.head(&b, "videos/abandoned.mp4").await.unwrap().is_none());
        empty_and_drop(&s3, &b).await;
    });
}

#[test]
fn presigned_urls_work_without_credentials() {
    let Some(cfg) = live() else { return };
    block(async move {
        let s3 = S3Client::new(cfg).unwrap();
        let b = bucket("pre");
        s3.create_bucket(&b).await.unwrap();
        let http = HttpClient::new();
        // A browser uploads straight to the store with a presigned PUT …
        let put = s3.presign_put(&b, "uploads/ảnh 1.jpg", Duration::from_secs(300)).unwrap();
        let r = http.send(Request::new(Method::Put, Url::parse(&put).unwrap()).body("image/jpeg", b"jpeg bytes".to_vec())).await.unwrap();
        assert!(r.is_success(), "presigned PUT: {} {}", r.status, r.text());
        // … and fetches it with a presigned GET.
        let get = s3.presign_get(&b, "uploads/ảnh 1.jpg", Duration::from_secs(300)).unwrap();
        let r = http.get(&get).await.unwrap();
        assert!(r.is_success(), "presigned GET: {} {}", r.status, r.text());
        assert_eq!(r.body, b"jpeg bytes");
        // A tampered signature, or no signature at all, is refused.
        let tampered = format!("{}0", &get[..get.len() - 1]);
        assert_eq!(http.get(&tampered).await.unwrap().status, 403);
        let plain = get.split('?').next().unwrap();
        assert_eq!(http.get(plain).await.unwrap().status, 403);
        empty_and_drop(&s3, &b).await;
    });
}

#[test]
fn failures_are_typed_errors() {
    let Some(cfg) = live() else { return };
    block(async move {
        let mut wrong = cfg.clone();
        wrong.credentials.secret_key = "not-the-secret".into();
        let e = S3Client::new(wrong).unwrap().bucket_exists("kwt-anything").await.unwrap_err();
        assert!(matches!(e, S3Error::Denied { .. }), "{e}");
        let mut wrong = cfg.clone();
        wrong.credentials.secret_key = "not-the-secret".into();
        let e = S3Client::new(wrong).unwrap().create_bucket(&bucket("denied")).await.unwrap_err();
        assert!(matches!(&e, S3Error::Denied { code, .. } if code == "SignatureDoesNotMatch"), "{e}");
        let mut gone = cfg.clone();
        gone.endpoint = "http://127.0.0.1:9".into();
        let e = S3Client::new(gone).unwrap().bucket_exists("kwt-anything").await.unwrap_err();
        assert!(matches!(e, S3Error::Connection(_)), "{e}");
        let s3 = S3Client::new(cfg).unwrap();
        let e = s3.get("kwt-no-such-bucket-at-all", "k").await.unwrap_err();
        assert!(matches!(&e, S3Error::NotFound { code, .. } if code == "NoSuchBucket"), "{e}");
    });
}
