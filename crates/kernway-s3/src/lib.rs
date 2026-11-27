//! # kernway-s3
//!
//! A self-written async client for S3-compatible object storage — AWS S3,
//! MinIO, Cloudflare R2, Backblaze B2, Wasabi … — on Kernway's own HTTP
//! client and runtime: AWS Signature V4 (checked against the AWS
//! documentation's vectors), buckets, objects with metadata and ranged
//! reads, server-side copies, paged listings, multipart uploads for large
//! files, and presigned GET / PUT URLs (a browser fetches or uploads a
//! salon's photo directly, the server only signs).
//!
//! ```rust,ignore
//! let s3 = S3Client::new(S3Config::new("http://127.0.0.1:9000", "minio", "minio-pw", "us-east-1"))?;
//! s3.create_bucket("pos-media").await?;
//! s3.put("pos-media", "avatars/42.jpg", jpeg, &PutOptions { content_type: Some("image/jpeg".into()), ..Default::default() }).await?;
//! let url = s3.presign_get("pos-media", "avatars/42.jpg", Duration::from_secs(600))?;
//! ```
//!
//! Everything is `!Send`, like the rest of the stack.

pub mod client;
pub mod sigv4;
pub mod xml;

pub use client::{ListPage, Object, ObjectInfo, PutOptions, S3Client, S3Config, S3Error, S3Result};
pub use sigv4::Credentials;
