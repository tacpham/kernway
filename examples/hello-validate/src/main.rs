//! Run the validation demo: `cargo run -p hello-validate`, then
//! `curl -X POST localhost:8080/users -d '{"name":"","email":"nope","age":200}'`.

fn main() -> std::io::Result<()> {
    hello_validate::build_app("0.0.0.0:8080").run()
}
