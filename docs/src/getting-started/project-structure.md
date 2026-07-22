# Project Structure

`````n
my-app/
├── Cargo.toml
├── config/
│   ├── application.toml        # base config
│   ├── application-dev.toml    # dev overrides
│   └── application-prod.toml  # prod overrides
└── src/
    ├── main.rs               # entry point
    ├── lib.rs                # module declarations
    ├── controller/           # #[controller] structs
    ├── service/              # #[component] business logic
    ├── repository/           # #[component] + DB access
    ├── model/
    │   └── dto/              # request/response types
    └── exception/            # AppError + #[exception_handler]
`````n
## See also

- [Your First App](first-app.md)
- [Building a REST API](../guides/rest-api.md)
