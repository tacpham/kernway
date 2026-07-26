//! Run: `cargo run -p hello-controller`, then
//!   curl localhost:8080/users/1
//!   curl -X DELETE localhost:8080/users/1                 # 403
//!   curl -X DELETE -H 'X-Role: ADMIN' localhost:8080/users/1   # 200

fn main() -> std::io::Result<()> {
    hello_controller::build_app("0.0.0.0:8080").run()
}
