# kernway-orm-core — JPA Compatibility Notes

> This document clearly describes what `kernway-orm-core` supports from JPA (JSR-338),  
> what it does not support, and why — the limitations come from the Rust language, not from design shortcomings.

---

## Overview

```
kernway-orm-core  ≈  JPA spec (JSR-338)
kernway-orm-sqlx  ≈  Hibernate (reference implementation)
```

**Fundamental differences**: JPA was designed for languages that have:
- **Runtime reflection** (Java has it, Rust does not)
- **Bytecode manipulation** (cglib/Javassist/ByteBuddy — Rust does not have this)
- **Garbage collection** (automatically manages lifetimes — Rust has a borrow checker)
- **Class inheritance** (Rust only has traits + enums, not a class hierarchy)

These differences require some JPA features to be redesigned — they cannot be copied 1:1.

---

## ✅ Fully supported

| JPA | kernway-orm-core | Notes |
|---|---|---|
| `@Entity` | `#[entity]` | Same |
| `@Table(name="...")` | `#[entity(table="...")]` | Same |
| `@Id` | `#[id]` | Same |
| `@GeneratedValue(AUTO)` | `#[id(strategy="auto")]` | Same |
| `@GeneratedValue(IDENTITY)` | `#[id(strategy="identity")]` | Same |
| `@GeneratedValue + UUID` | `#[id(strategy="uuid")]` | Same |
| `@Column` | `#[column]` | Same |
| `@Column(name, nullable, unique)` | `#[column(name, nullable, unique)]` | Same |
| `@Column(insertable=false, updatable=false)` | `#[column(auto)]` | Equivalent — for `created_at`, `updated_at` |
| `@OneToMany` | `#[one_to_many(mapped_by="...")]` | Same |
| `@ManyToOne` | `#[many_to_one(column="...")]` | Same |
| `@ManyToMany` | `#[many_to_many(join_table="...")]` | Same |
| `@Embedded` / `@Embeddable` | `#[embedded]` | Same |
| `@Version` | `#[version]` | Optimistic locking — same |
| `@Transactional` | `#[transactional]` | Same |
| `JpaRepository<T, ID>` | `Repository<T>` trait | Equivalent |
| `findByEmailAndActive(...)` | auto-generated from `#[repository]` | Equivalent |
| `@Query("SELECT ...")` | `query!("SELECT ...")` macro | Equivalent |
| `@PrePersist` / `@PostPersist` | `#[pre_persist]` / `#[post_persist]` | Lifecycle hooks — same |
| `@PreUpdate` / `@PostUpdate` | `#[pre_update]` / `#[post_update]` | Same |
| `@PreRemove` / `@PostRemove` | `#[pre_remove]` / `#[post_remove]` | Same |
| `CascadeType.PERSIST` | `#[cascade(save)]` | Equivalent |
| `CascadeType.REMOVE` | `#[cascade(delete)]` | Equivalent |
| `CascadeType.ALL` | `#[cascade(all)]` | Equivalent |
| `Specification<T>` (Spring Data) | `QueryBuilder<T>` | Equivalent — lambda instead of a Predicate class |
| `Pageable` / `Page<T>` | `fetch_page(page, size) → Page<T>` | Equivalent |
| `Sort` | `.order_by_asc()` / `.order_by_desc()` | Equivalent |
| `@MappedSuperclass` | Rust trait / struct composition | Different syntax, same purpose |
| `@Enumerated(STRING)` | Automatic (Rust enum serialization = string) | More natural in Rust |
| `@SequenceGenerator` | `#[id(strategy="sequence", name="...")]` | Database-specific |

---

## ⚠️ Supported differently — requires a different mental model

### 1. Lazy Loading → Explicit `.with()`

**JPA:**
```java
@OneToMany(fetch = FetchType.LAZY)
private List<Post> posts;

// Truy cập field → Hibernate tự động chạy thêm 1 query (proxy)
user.getPosts();   // SELECT * FROM posts WHERE user_id = ?
```

**kernway-orm-core:**
```rust
// Lazy loading KHÔNG THỂ — Rust không có bytecode proxy

// Phải khai báo rõ khi nào cần load:
repo.query()
    .filter(|u| u.id == id)
    .with("posts")       // → LEFT JOIN posts ON posts.user_id = users.id
    .fetch_one()
    .await

// Hoặc nếu không cần posts, không load → không có N+1 problem
let user = repo.find_by_id(&id).await?;   // posts = Vec::new() (empty)
```

**Why?** Rust does not have runtime bytecode manipulation (cglib/Javassist).  
Hibernate creates proxy classes at runtime to intercept field access. Rust is compile-time only, so this is not possible.

**Advantage of the kernway approach**: There is no hidden N+1 behavior. Developers always know which queries will run.

---

### 2. @Inheritance Mapping → Enum Variants

**JPA:**
```java
@Entity
@Inheritance(strategy = InheritanceType.SINGLE_TABLE)
@DiscriminatorColumn(name = "type")
public abstract class Payment { ... }

@Entity
@DiscriminatorValue("CREDIT")
public class CreditPayment extends Payment { ... }

@Entity
@DiscriminatorValue("PAYPAL")
public class PaypalPayment extends Payment { ... }
```

**kernway-orm-core:**
```rust
// Rust không có class inheritance → dùng enum variants

#[entity(table = "payments")]
pub struct Payment {
    #[id] pub id: u64,
    pub amount: Decimal,

    // Discriminator column tự xử lý bởi enum
    #[column(name = "type")]
    pub kind: PaymentKind,
}

#[derive(Serialize, Deserialize)]
pub enum PaymentKind {
    Credit { card_last4: String },
    Paypal { email: String },
    BankTransfer { iban: String },
}
// → lưu dưới dạng JSON column hoặc separate table tùy config
```

**Explanation**: A Rust enum with data fields is a natural fit for the `SINGLE_TABLE` strategy.  
The `TABLE_PER_CLASS` strategy requires separate entity structs because inheritance is not available.

---

### 3. EntityManager → Repository<T>

**JPA:**
```java
@PersistenceContext
EntityManager em;

em.persist(entity);
em.find(User.class, id);
em.createQuery("SELECT u FROM User u WHERE u.email = :email")
  .setParameter("email", email).getResultList();
em.flush();
em.clear();
```

**kernway-orm-core:**
```rust
// Không có EntityManager — Repository<T> thay thế hoàn toàn
// DI inject trực tiếp repository

#[inject] repo: Arc<UserRepository>,

repo.save(entity).await?;
repo.find_by_id(&id).await?;
repo.find_by_email(&email).await?;    // method-name query
```

**Why?** `EntityManager` is stateful (first-level cache, dirty tracking, identity map).  
Rust's ownership model does not fit stateful entity tracking well.  
`Repository<T>` is stateless, which fits Rust better.

---

### 4. JPQL → Lambda + Raw SQL

**JPA (JPQL):**
```java
@Query("SELECT u FROM User u WHERE u.email LIKE :domain AND u.active = true ORDER BY u.createdAt DESC")
List<User> findByDomain(@Param("domain") String domain);
```

**kernway-orm-core:**
```rust
// Option 1: Lambda (type-safe, không SQL injection)
repo.query()
    .filter(|u| u.email.ends_with("@gmail.com") && u.active == true)
    .order_by_desc(|u| u.created_at)
    .fetch_all()
    .await

// Option 2: Raw SQL macro (khi cần complex query)
#[repository(User)]
impl UserRepository {
    #[query("SELECT * FROM users WHERE email LIKE $1 AND active = true ORDER BY created_at DESC")]
    async fn find_by_domain(&self, domain: &str) -> Result<Vec<User>, OrmError>;
}
```

**Why no JPQL?** JPQL is a query language built around Java class names. Rust does not have an equivalent notion of runtime class names. Lambdas are more type-safe and have better IDE support.

---

### 5. L2 Cache → kernway-cache (separate)

**JPA:**
```java
@Entity
@Cacheable   // Hibernate L2 cache tích hợp
public class User { ... }
```

**kernway-orm-core:**
```rust
// kernway-orm không có built-in L2 cache
// Dùng kernway-cache riêng (tường minh hơn)

#[cacheable(key = "user:{id}", ttl = 300)]
pub async fn find_user(&self, id: u64) -> Result<Option<User>, AppError> {
    self.repo.find_by_id(&id).await.map_err(Into::into)
}
```

**Why separate it?** Hibernate's integrated L2 cache can be difficult to debug (stale data, cache invalidation). An explicit separate layer is clearer.

---

## ❌ Not supported — Rust limitations

| JPA Feature | Reason unsupported |
|---|---|
| **Lazy loading proxy** | Requires bytecode manipulation (cglib/Javassist). Rust is compile-time only and has no runtime code generation. |
| **Dirty tracking** (EntityManager flush) | Requires runtime proxies to track field mutations. Rust ownership does not allow "observe mutation after the fact" behavior. |
| **First-level cache (Identity Map)** | EntityManager is a stateful cache. Rust has no GC, so lifetimes become complex. |
| **JPQL / HQL** | Java class names do not exist at runtime in Rust. |
| **`@DynamicUpdate`** (only update changed fields) | Requires dirty tracking — see above. |
| **Detached/Managed/Removed entity states** | These are EntityManager lifecycle states. Rust uses a stateless repository pattern. |
| **`@SecondaryTable`** | Maps one entity across multiple tables. Too complex and rarely used. |

---

## Quick mapping table for Spring developers

```
Spring @Entity          →  #[entity]
Spring @Id              →  #[id]
Spring @Column          →  #[column]
Spring @OneToMany       →  #[one_to_many]
Spring @ManyToOne       →  #[many_to_one]
Spring @Transactional   →  #[transactional]
Spring JpaRepository    →  Repository<T> trait
Spring findByEmailAnd.. →  auto-generated from #[repository]
Spring @Query("SELECT") →  #[query("SELECT")]
Spring FetchType.EAGER  →  .with("relation")
Spring FetchType.LAZY   →  NOT AVAILABLE → use .with() when needed
Spring Specification<T> →  QueryBuilder<T> lambda
Spring Page<T>/Pageable →  fetch_page(page, size) → Page<T>
Spring @Cacheable       →  #[cacheable] in kernway-cache
Spring EntityManager    →  NOT AVAILABLE → Repository<T> is sufficient
Spring JPQL             →  NOT AVAILABLE → lambda + raw SQL macro
```

---

## Conclusion

**kernway-orm-core achieves ~85% JPA compatibility for real-world use cases.**

The remaining 15% consists of features that are either rarely used or problematic even in JPA:
- Lazy loading → the well-known N+1 problem in Hibernate
- Dirty tracking / EntityManager → the root cause of "LazyInitializationException"
- JPQL → runtime errors because it is string-based

kernway-orm-core solves these issues with a different approach that is **more idiomatic to Rust** and safer.
