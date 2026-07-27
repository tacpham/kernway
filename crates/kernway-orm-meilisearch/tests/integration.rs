//! # Meilisearch Integration Tests
//!
//! These tests require a live Meilisearch instance. Start one with:
//!
//! ```bash
//! docker compose up -d          # from crates/kernway-orm-meilisearch/
//! ```
//!
//! Then run:
//! ```bash
//! cargo test -p kernway-orm-meilisearch --features meilisearch --test integration -- --test-threads=1
//! ```
//!
//! Environment variables (optional — defaults to localhost):
//! - `MEILI_URL`     — default `http://localhost:7700`
//! - `MEILI_API_KEY` — default `testmasterkey`
//!
//! Each test cleans up its own index so tests are independent.

#![cfg(feature = "meilisearch")]

use kernway_orm_core::{
    entity::Entity,
    query::QueryBuilder,
    repository::Repository,
    driver::Driver,
};
use kernway_orm_meilisearch::{
    api,
    driver::{MeilisearchConfig, MeilisearchDriver},
};
use kernway_orm_macro::entity;
use rt_core::Executor;
use serde::{Deserialize, Serialize};

// ── Test helpers ──────────────────────────────────────────────────────────────

fn meili_url() -> String {
    std::env::var("MEILI_URL").unwrap_or_else(|_| "http://localhost:7700".into())
}

fn meili_key() -> String {
    std::env::var("MEILI_API_KEY").unwrap_or_else(|_| "testmasterkey".into())
}

fn driver() -> MeilisearchDriver {
    MeilisearchDriver::connect(meili_url(), meili_key())
}

use kernway_orm_core::error::OrmError;

type TestResult = Result<(), OrmError>;

/// Run an async block on a fresh Kernway executor.
macro_rules! run {
    ($body:expr) => {{
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = TestResult>>> =
            Box::pin($body);
        Executor::new()
            .unwrap()
            .block_on(fut)
            .expect("executor panicked")
            .unwrap_or_else(|e| panic!("test failed: {e}"));
    }};
}

/// Drop index, ignoring errors (cleanup helper).
async fn cleanup(index: &str) {
    let _ = api::drop_index(&meili_url(), &meili_key(), index).await;
}

// ── Entities ──────────────────────────────────────────────────────────────────

/// Simple product with an integer primary key.
#[entity(table = "products_test")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Product {
    #[id()]
    id: u64,
    name: String,
    price: f64,
    category: String,
    stock: u32,
}

/// Entity with a **string UUID** as the primary key (custom id type).
#[entity(table = "articles_test")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Article {
    #[id()]
    slug: String,
    title: String,
    views: u64,
    published: bool,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// 1. ping — server must be up
#[test]
fn test_ping() {
    run!(async {
        driver().ping().await
    });
}

/// 2. save + find_by_id round-trip
#[test]
fn test_save_and_find_by_id() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        let saved = repo.save(Product {
            id: 1,
            name: "Widget".into(),
            price: 9.99,
            category: "tools".into(),
            stock: 100,
        }).await?;
        assert_eq!(saved.id, 1);
        assert_eq!(saved.name, "Widget");

        let found = repo.find_by_id(&1).await?.expect("should exist");
        assert_eq!(found.id, 1);
        assert!((found.price - 9.99).abs() < 0.001);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 3. find_by_id returns None for a missing document
#[test]
fn test_find_by_id_missing() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        let result = repo.find_by_id(&999).await?;
        assert!(result.is_none(), "should return None for missing id");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 4. save_all + find_all (batch insert)
#[test]
fn test_save_all_and_find_all() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        let batch = vec![
            Product { id: 10, name: "A".into(), price: 1.0, category: "x".into(), stock: 5 },
            Product { id: 11, name: "B".into(), price: 2.0, category: "x".into(), stock: 10 },
            Product { id: 12, name: "C".into(), price: 3.0, category: "y".into(), stock: 0 },
        ];
        repo.save_all(batch).await?;

        let all = repo.find_all().await?;
        assert_eq!(all.len(), 3);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 5. count
#[test]
fn test_count() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        assert_eq!(repo.count().await?, 0);
        repo.save(Product { id: 1, name: "A".into(), price: 1.0, category: "x".into(), stock: 1 }).await?;
        repo.save(Product { id: 2, name: "B".into(), price: 2.0, category: "y".into(), stock: 2 }).await?;
        assert_eq!(repo.count().await?, 2);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 6. exists_by_id
#[test]
fn test_exists_by_id() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        assert!(!repo.exists_by_id(&42).await?);
        repo.save(Product { id: 42, name: "X".into(), price: 0.0, category: "z".into(), stock: 0 }).await?;
        assert!(repo.exists_by_id(&42).await?);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 7. delete_by_id
#[test]
fn test_delete_by_id() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save(Product { id: 5, name: "Del".into(), price: 0.0, category: "z".into(), stock: 0 }).await?;
        assert!(repo.exists_by_id(&5).await?);
        repo.delete_by_id(&5).await?;
        assert!(!repo.exists_by_id(&5).await?);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 8. find_all_by_ids
#[test]
fn test_find_all_by_ids() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "A".into(), price: 1.0, category: "x".into(), stock: 1 },
            Product { id: 2, name: "B".into(), price: 2.0, category: "x".into(), stock: 2 },
            Product { id: 3, name: "C".into(), price: 3.0, category: "y".into(), stock: 3 },
        ]).await?;
        let found = repo.find_all_by_ids(&[1, 3]).await?;
        let mut ids: Vec<u64> = found.iter().map(|p| p.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 3]);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 9. delete_all_by_ids
#[test]
fn test_delete_all_by_ids() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "A".into(), price: 1.0, category: "x".into(), stock: 1 },
            Product { id: 2, name: "B".into(), price: 2.0, category: "x".into(), stock: 2 },
            Product { id: 3, name: "C".into(), price: 3.0, category: "y".into(), stock: 3 },
        ]).await?;
        repo.delete_all_by_ids(&[1, 2]).await?;
        assert_eq!(repo.count().await?, 1);
        assert!(repo.exists_by_id(&3).await?);
        cleanup("products_test").await;
        Ok(())
    });
}

// ── Index settings ─────────────────────────────────────────────────────────────

/// 10. Configure filterable attributes, then filter with filter_eq
#[test]
fn test_filterable_attributes_and_filter_eq() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();

        // Ensure index exists before configuring settings
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["category", "stock"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "Hammer".into(), price: 12.5, category: "tools".into(), stock: 50 },
            Product { id: 2, name: "Nails".into(), price: 3.0, category: "tools".into(), stock: 200 },
            Product { id: 3, name: "Paint".into(), price: 8.0, category: "decor".into(), stock: 30 },
        ]).await?;

        let results = repo.query()
            .filter_eq("category", "tools")
            .fetch_all()
            .await?;
        assert_eq!(results.len(), 2, "expected 2 tools");
        for p in &results {
            assert_eq!(p.category, "tools");
        }
        cleanup("products_test").await;
        Ok(())
    });
}

/// 11. filter_gt / filter_lt on a numeric field
#[test]
fn test_filter_gt_lt() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["price"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "Cheap".into(),     price: 2.0,  category: "x".into(), stock: 1 },
            Product { id: 2, name: "Mid".into(),       price: 10.0, category: "x".into(), stock: 1 },
            Product { id: 3, name: "Expensive".into(), price: 50.0, category: "x".into(), stock: 1 },
        ]).await?;

        let gt5 = repo.query().filter_gt("price", "5").fetch_all().await?;
        assert_eq!(gt5.len(), 2, "price > 5 should return 2");

        let lt20 = repo.query().filter_lt("price", "20").fetch_all().await?;
        assert_eq!(lt20.len(), 2, "price < 20 should return 2");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 12. filter_between
#[test]
fn test_filter_between() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["price"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "A".into(), price: 5.0,  category: "x".into(), stock: 1 },
            Product { id: 2, name: "B".into(), price: 15.0, category: "x".into(), stock: 1 },
            Product { id: 3, name: "C".into(), price: 25.0, category: "x".into(), stock: 1 },
            Product { id: 4, name: "D".into(), price: 35.0, category: "x".into(), stock: 1 },
        ]).await?;

        // Meilisearch BETWEEN syntax: "field 10 TO 30"
        let results = repo.query()
            .filter_between("price", "10", "30")
            .fetch_all()
            .await?;
        assert_eq!(results.len(), 2, "price 10 TO 30 should return 2");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 13. filter_in
#[test]
fn test_filter_in() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["category"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "A".into(), price: 1.0, category: "tools".into(),  stock: 1 },
            Product { id: 2, name: "B".into(), price: 2.0, category: "decor".into(),  stock: 1 },
            Product { id: 3, name: "C".into(), price: 3.0, category: "garden".into(), stock: 1 },
            Product { id: 4, name: "D".into(), price: 4.0, category: "other".into(),  stock: 1 },
        ]).await?;

        let results = repo.query()
            .filter_in("category", vec!["tools".into(), "garden".into()])
            .fetch_all()
            .await?;
        assert_eq!(results.len(), 2);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 14. full-text search via filter_like (maps to Meilisearch `q` param)
#[test]
fn test_filter_like_fulltext() {
    run!(async {
        cleanup("products_test").await;
        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "Electric Drill".into(),  price: 59.0, category: "tools".into(), stock: 5 },
            Product { id: 2, name: "Paint Roller".into(),    price: 8.0,  category: "decor".into(), stock: 20 },
            Product { id: 3, name: "Electric Sander".into(), price: 79.0, category: "tools".into(), stock: 3 },
        ]).await?;

        let results = repo.query()
            .filter_like("name", "Electric")
            .fetch_all()
            .await?;
        // Full-text search — should match both "Electric Drill" and "Electric Sander"
        assert_eq!(results.len(), 2, "full-text 'Electric' should match 2");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 15. Sortable attributes + order_by
#[test]
fn test_sortable_attributes_and_order_by() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_sortable_attributes(&url, &key, "products_test", &["price"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "C".into(), price: 30.0, category: "x".into(), stock: 1 },
            Product { id: 2, name: "A".into(), price: 10.0, category: "x".into(), stock: 1 },
            Product { id: 3, name: "B".into(), price: 20.0, category: "x".into(), stock: 1 },
        ]).await?;

        let asc = repo.query().order_by_asc("price").fetch_all().await?;
        let prices_asc: Vec<f64> = asc.iter().map(|p| p.price).collect();
        assert_eq!(prices_asc, vec![10.0, 20.0, 30.0], "should be ascending");

        let desc = repo.query().order_by_desc("price").fetch_all().await?;
        let prices_desc: Vec<f64> = desc.iter().map(|p| p.price).collect();
        assert_eq!(prices_desc, vec![30.0, 20.0, 10.0], "should be descending");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 16. limit + offset (pagination)
#[test]
fn test_limit_and_offset() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_sortable_attributes(&url, &key, "products_test", &["id"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all((1u64..=10).map(|i| Product {
            id: i, name: format!("P{i}"), price: i as f64, category: "x".into(), stock: 1,
        }).collect()).await?;

        let page1 = repo.query().order_by_asc("id").limit(3).offset(0).fetch_all().await?;
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].id, 1);

        let page2 = repo.query().order_by_asc("id").limit(3).offset(3).fetch_all().await?;
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].id, 4);
        cleanup("products_test").await;
        Ok(())
    });
}

/// 17. Update maxTotalHits (pagination settings)
#[test]
fn test_set_pagination_max_total_hits() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;

        // Lower maxTotalHits to 5
        api::set_pagination(&url, &key, "products_test", 5).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all((1u64..=10).map(|i| Product {
            id: i, name: format!("P{i}"), price: i as f64, category: "x".into(), stock: 1,
        }).collect()).await?;

        // With maxTotalHits = 5, search returns at most 5 hits
        let all = repo.query().limit(100).fetch_all().await?;
        assert!(all.len() <= 5, "maxTotalHits=5 should cap results at 5, got {}", all.len());
        cleanup("products_test").await;
        Ok(())
    });
}

/// 18. Custom string UUID primary key
#[test]
fn test_custom_string_id() {
    run!(async {
        cleanup("articles_test").await;
        let repo: Box<dyn Repository<Article>> = driver().repository();
        repo.save(Article {
            slug: "hello-world".into(),
            title: "Hello World".into(),
            views: 100,
            published: true,
        }).await?;
        repo.save(Article {
            slug: "rust-async".into(),
            title: "Rust Async".into(),
            views: 500,
            published: false,
        }).await?;

        let found = repo.find_by_id(&"hello-world".to_string()).await?.expect("should find by slug");
        assert_eq!(found.title, "Hello World");

        let missing = repo.find_by_id(&"does-not-exist".to_string()).await?;
        assert!(missing.is_none());
        cleanup("articles_test").await;
        Ok(())
    });
}

/// 19. filter_is_null / filter_is_not_null (filterable field must exist)
#[test]
fn test_filter_gte_lte() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["stock"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "A".into(), price: 1.0, category: "x".into(), stock: 0 },
            Product { id: 2, name: "B".into(), price: 2.0, category: "x".into(), stock: 5 },
            Product { id: 3, name: "C".into(), price: 3.0, category: "x".into(), stock: 10 },
            Product { id: 4, name: "D".into(), price: 4.0, category: "x".into(), stock: 20 },
        ]).await?;

        let gte5 = repo.query().filter_gte("stock", "5").fetch_all().await?;
        assert_eq!(gte5.len(), 3, "stock >= 5 should return 3");

        let lte10 = repo.query().filter_lte("stock", "10").fetch_all().await?;
        assert_eq!(lte10.len(), 3, "stock <= 10 should return 3");
        cleanup("products_test").await;
        Ok(())
    });
}

/// 20. Combined filter: category + price range
#[test]
fn test_combined_filters() {
    run!(async {
        cleanup("products_test").await;
        let url = meili_url();
        let key = meili_key();
        api::ensure_index(&url, &key, "products_test", "id").await?;
        api::set_filterable_attributes(&url, &key, "products_test", &["category", "price"]).await?;

        let repo: Box<dyn Repository<Product>> = driver().repository();
        repo.save_all(vec![
            Product { id: 1, name: "Cheap Tool".into(),     price: 5.0,  category: "tools".into(), stock: 1 },
            Product { id: 2, name: "Expensive Tool".into(), price: 100.0, category: "tools".into(), stock: 1 },
            Product { id: 3, name: "Cheap Decor".into(),    price: 5.0,  category: "decor".into(), stock: 1 },
            Product { id: 4, name: "Mid Tool".into(),       price: 25.0, category: "tools".into(), stock: 1 },
        ]).await?;

        let results = repo.query()
            .filter_eq("category", "tools")
            .filter_lt("price", "50")
            .fetch_all()
            .await?;
        assert_eq!(results.len(), 2, "tools with price < 50: expected 2");
        for p in &results {
            assert_eq!(p.category, "tools");
            assert!(p.price < 50.0);
        }
        cleanup("products_test").await;
        Ok(())
    });
}
