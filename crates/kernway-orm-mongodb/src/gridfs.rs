//! GridFS: files larger than a document, stored as `<bucket>.files`
//! (metadata) and `<bucket>.chunks` (255 KiB pieces), per the GridFS spec.

use crate::bson::{Binary, Bson, DateTime, Document, ObjectId};
use crate::collection::{Collection, Database, FindOptions, IndexModel};
use crate::doc;
use crate::error::{Error, Result};

/// A GridFS bucket.
#[derive(Clone)]
pub struct GridFsBucket {
    files: Collection<Document>,
    chunks: Collection<Document>,
    chunk_size: usize,
}

/// Default chunk size: 255 KiB.
pub const DEFAULT_CHUNK_SIZE: usize = 255 * 1024;

impl Database {
    /// The GridFS bucket `name` (`"fs"` by convention).
    pub fn gridfs(&self, name: &str) -> GridFsBucket {
        GridFsBucket {
            files: self.collection(&format!("{name}.files")),
            chunks: self.collection(&format!("{name}.chunks")),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

impl GridFsBucket {
    /// With another chunk size.
    pub fn with_chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes.max(1);
        self
    }

    async fn ensure_indexes(&self) -> Result<()> {
        // Only when the bucket is new (index builds take locks).
        if self.files.find_one(Document::new(), FindOptions { projection: Some(doc! { "_id" => 1 }), ..Default::default() }).await?.is_some() {
            return Ok(());
        }
        self.files.create_index(IndexModel { keys: doc! { "filename" => 1, "uploadDate" => 1 }, ..Default::default() }).await?;
        self.chunks.create_index(IndexModel { keys: doc! { "files_id" => 1, "n" => 1 }, unique: true, ..Default::default() }).await?;
        Ok(())
    }

    /// Store `data` as `filename` (with optional metadata); returns its id.
    pub async fn upload(&self, filename: &str, data: &[u8], metadata: Option<Document>) -> Result<ObjectId> {
        self.ensure_indexes().await?;
        let id = ObjectId::new();
        let chunks: Vec<Document> = data
            .chunks(self.chunk_size)
            .enumerate()
            .map(|(n, c)| doc! { "_id" => ObjectId::new(), "files_id" => id, "n" => n as i32, "data" => Binary { subtype: 0, bytes: c.to_vec() } })
            .collect();
        if !chunks.is_empty() {
            if let Err(e) = self.chunks.insert_many_documents(chunks, true).await {
                let _ = self.chunks.delete_many(doc! { "files_id" => id }).await;
                return Err(e);
            }
        }
        let mut file = doc! {
            "_id" => id,
            "length" => data.len() as i64,
            "chunkSize" => self.chunk_size as i32,
            "uploadDate" => DateTime::now(),
            "filename" => filename,
        };
        if let Some(m) = metadata {
            file.insert("metadata", m);
        }
        self.files.insert_many_documents(vec![file], true).await?;
        Ok(id)
    }

    /// The file's bytes, checking every chunk is present and sized right.
    pub async fn download(&self, id: ObjectId) -> Result<Vec<u8>> {
        let file = self.files.find_one(doc! { "_id" => id }, FindOptions::default()).await?.ok_or_else(|| Error::config(format!("no GridFS file {id}")))?;
        self.read_file(&file).await
    }

    /// The newest revision of `filename` (`revision` -1), the oldest (0), …
    pub async fn download_by_name(&self, filename: &str, revision: i64) -> Result<Vec<u8>> {
        let (skip, dir) = if revision >= 0 { (revision as u64, 1) } else { ((-revision - 1) as u64, -1) };
        let o = FindOptions { sort: Some(doc! { "uploadDate" => dir }), skip: Some(skip), ..Default::default() };
        let file = self.files.find_one(doc! { "filename" => filename }, o).await?.ok_or_else(|| Error::config(format!("no GridFS file {filename:?} revision {revision}")))?;
        self.read_file(&file).await
    }

    async fn read_file(&self, file: &Document) -> Result<Vec<u8>> {
        let id = file.get("_id").cloned().unwrap_or(Bson::Null);
        let length = file.get_i64("length").unwrap_or(0) as usize;
        let chunk_size = file.get_i64("chunkSize").unwrap_or(DEFAULT_CHUNK_SIZE as i64).max(1) as usize;
        let expected = length.div_ceil(chunk_size);
        let mut out = Vec::with_capacity(length);
        let mut cur = self.chunks.find(doc! { "files_id" => id }, FindOptions { sort: Some(doc! { "n" => 1 }), ..Default::default() }).await?;
        let mut n = 0usize;
        while let Some(c) = cur.next().await {
            let c = c?;
            if c.get_i64("n") != Some(n as i64) {
                return Err(Error::protocol(format!("GridFS chunk {n} missing")));
            }
            let Some(Bson::Binary(b)) = c.get("data") else { return Err(Error::protocol("GridFS chunk without data")) };
            let want = if n + 1 == expected { length - n * chunk_size } else { chunk_size };
            if b.bytes.len() != want {
                return Err(Error::protocol(format!("GridFS chunk {n} has {} bytes, {want} expected", b.bytes.len())));
            }
            out.extend_from_slice(&b.bytes);
            n += 1;
        }
        if n != expected {
            return Err(Error::protocol(format!("GridFS file has {n} of {expected} chunks")));
        }
        Ok(out)
    }

    /// Files whose metadata documents match `filter`.
    pub async fn find(&self, filter: Document) -> Result<Vec<Document>> {
        self.files.find(filter, FindOptions::default()).await?.collect().await
    }

    /// Delete a file and its chunks.
    pub async fn delete(&self, id: ObjectId) -> Result<()> {
        let n = self.files.delete_one(doc! { "_id" => id }).await?;
        self.chunks.delete_many(doc! { "files_id" => id }).await?;
        if n == 0 {
            return Err(Error::config(format!("no GridFS file {id}")));
        }
        Ok(())
    }

    /// Rename a file.
    pub async fn rename(&self, id: ObjectId, new_name: &str) -> Result<()> {
        let r = self.files.update_one(doc! { "_id" => id }, doc! { "$set" => doc! { "filename" => new_name } }, Default::default()).await?;
        if r.matched == 0 {
            return Err(Error::config(format!("no GridFS file {id}")));
        }
        Ok(())
    }
}
