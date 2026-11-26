//! `#[repository]` derived queries, run against the in-memory driver: each
//! method name becomes the query it promises — prefix, AND-joined conditions,
//! every operator, parameters bound left to right, and the result shape picked
//! from the return type.
#![allow(async_fn_in_trait)]

use kernway_orm_core::error::OrmError;
use kernway_orm_core::repository::Repository;
use kernway_orm_macro::{entity, repository};
use kernway_orm_memory::InMemoryRepository;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "users")]
struct User {
    #[id]
    id: u64,
    email: String,
    role: String,
    age: i64,
    first_name: String,
}

#[repository(entity = User)]
trait UserRepo {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, OrmError>;
    async fn find_all_by_role(&self, role: &str) -> Result<Vec<User>, OrmError>;
    async fn find_by_role_and_age_gt(&self, role: &str, age: i64) -> Result<Vec<User>, OrmError>;
    async fn find_by_age_gte_and_age_lte(&self, low: i64, high: i64) -> Result<Vec<User>, OrmError>;
    async fn find_by_age_lt(&self, age: i64) -> Result<Vec<User>, OrmError>;
    async fn find_by_role_ne(&self, role: &str) -> Result<Vec<User>, OrmError>;
    async fn find_by_email_like(&self, part: &str) -> Result<Vec<User>, OrmError>;
    async fn find_by_first_name(&self, name: String) -> Result<Vec<User>, OrmError>;
    async fn count_by_role(&self, role: &str) -> Result<u64, OrmError>;
    async fn exists_by_email(&self, email: &str) -> Result<bool, OrmError>;
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

fn user(id: u64, email: &str, role: &str, age: i64, first: &str) -> User {
    User { id, email: email.into(), role: role.into(), age, first_name: first.into() }
}

fn seeded() -> UserRepoImpl {
    let mem = InMemoryRepository::<User>::new();
    run(async {
        for u in [
            user(1, "ann@a.io", "ADMIN", 41, "Ann"),
            user(2, "bob@b.io", "USER", 17, "Bob"),
            user(3, "cy@a.io", "USER", 30, "Cy"),
            user(4, "dee@d.io", "ADMIN", 9, "Dee Dee"),
        ] {
            mem.save(u).await.unwrap();
        }
    });
    UserRepoImpl::new(Box::new(mem))
}

fn ids(mut v: Vec<User>) -> Vec<u64> {
    v.sort_by_key(|u| u.id);
    v.into_iter().map(|u| u.id).collect()
}

#[test]
fn find_by_with_an_option_return_fetches_one() {
    let repo = seeded();
    run(async {
        assert_eq!(repo.find_by_email("cy@a.io").await.unwrap().map(|u| u.id), Some(3));
        assert_eq!(repo.find_by_email("nobody@x").await.unwrap(), None);
    });
}

#[test]
fn find_all_by_and_a_vec_return_fetch_every_match() {
    let repo = seeded();
    run(async {
        assert_eq!(ids(repo.find_all_by_role("ADMIN").await.unwrap()), [1, 4]);
        assert!(repo.find_all_by_role("GUEST").await.unwrap().is_empty());
    });
}

#[test]
fn and_joined_conditions_bind_parameters_left_to_right() {
    let repo = seeded();
    run(async {
        assert_eq!(ids(repo.find_by_role_and_age_gt("USER", 18).await.unwrap()), [3]);
        assert_eq!(ids(repo.find_by_role_and_age_gt("ADMIN", 18).await.unwrap()), [1]);
        // Same field twice, two operators: low binds to gte, high to lte.
        assert_eq!(ids(repo.find_by_age_gte_and_age_lte(17, 30).await.unwrap()), [2, 3]);
        assert!(repo.find_by_age_gte_and_age_lte(30, 17).await.unwrap().is_empty());
    });
}

#[test]
fn every_operator_suffix_maps_to_its_filter() {
    let repo = seeded();
    run(async {
        assert_eq!(ids(repo.find_by_age_lt(17).await.unwrap()), [4], "lt is strict (9 < 17, 17 is not)");
        assert_eq!(ids(repo.find_by_role_ne("ADMIN").await.unwrap()), [2, 3]);
        assert_eq!(ids(repo.find_by_email_like("@a.io").await.unwrap()), [1, 3]);
    });
}

#[test]
fn a_multi_word_field_and_an_owned_parameter() {
    let repo = seeded();
    run(async {
        assert_eq!(ids(repo.find_by_first_name("Dee Dee".to_string()).await.unwrap()), [4]);
    });
}

#[test]
fn count_by_and_exists_by() {
    let repo = seeded();
    run(async {
        assert_eq!(repo.count_by_role("USER").await.unwrap(), 2);
        assert_eq!(repo.count_by_role("GUEST").await.unwrap(), 0);
        assert!(repo.exists_by_email("bob@b.io").await.unwrap());
        assert!(!repo.exists_by_email("bob@b.com").await.unwrap());
    });
}
