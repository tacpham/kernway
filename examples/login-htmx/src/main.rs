//! Runs the login-htmx walking skeleton. See the crate lib for the flow.
//!
//!   cargo run -p login-htmx
//!   open http://localhost:8080/login

fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    login_htmx::build_app(&format!("0.0.0.0:{port}")).run()
}
