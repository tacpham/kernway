use proc_macro::TokenStream;

/// Marks a function as cacheable.
///
/// # Arguments
/// - `key` — cache key expression (string, evaluated at call site)
/// - `ttl` — TTL in seconds (default: 300)
/// - `region` — cache region name (default: "default")
///
/// # Example
/// ```rust,ignore
/// #[cacheable(key = "user_{id}", ttl = 60)]
/// pub fn get_user(&self, id: u64) -> Option<User> { ... }
/// ```
///
/// In v0.5, this is a marker macro (records intent, full AOP in v0.6).
#[proc_macro_attribute]
pub fn cacheable(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Evicts cache entries after the annotated function executes.
///
/// # Arguments
/// - `key` — cache key expression to evict
/// - `region` — cache region (default: "default")
/// - `all_entries` — if true, clears the entire region
///
/// # Example
/// ```rust,ignore
/// #[cache_evict(key = "user_{id}")]
/// pub fn update_user(&self, id: u64, data: UserUpdate) -> User { ... }
/// ```
#[proc_macro_attribute]
pub fn cache_evict(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Updates the cache with the return value after function executes.
///
/// # Arguments
/// - `key` — cache key expression
/// - `ttl` — TTL in seconds (default: 300)
/// - `region` — cache region (default: "default")
///
/// # Example
/// ```rust,ignore
/// #[cache_update(key = "user_{id}", ttl = 60)]
/// pub fn save_user(&self, id: u64, data: UserUpdate) -> User { ... }
/// ```
#[proc_macro_attribute]
pub fn cache_update(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}
