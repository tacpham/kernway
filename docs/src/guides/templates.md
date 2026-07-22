# kernleaf — Template Engine

## Purpose

A Thymeleaf-inspired server-side template engine. Attribute-based syntax. Automatic XSS escaping.

## Standards

- **WHATWG HTML Living Standard** — valid HTML output
- **OWASP XSS Prevention Cheat Sheet** — auto-escape, raw output explicit
- **OWASP CSRF Cheat Sheet** — auto CSRF token injection

## Syntax

```html
<!DOCTYPE html>
<html>
<body>
    <!-- Text output — HTML-escaped mặc định (XSS safe) -->
    <h1 kw:text="${user.name}">Placeholder Name</h1>

    <!-- Attribute binding -->
    <input type="text" kw:value="${user.email}" name="email">

    <!-- Conditional rendering -->
    <div kw:if="${user.isAdmin}">Admin panel</div>
    <div kw:unless="${user.isAdmin}">Regular user</div>

    <!-- Loop -->
    <ul>
        <li kw:each="post : ${posts}" kw:text="${post.title}">Post title</li>
    </ul>

    <!-- Security: only show to specific roles -->
    <a kw:authorize="hasRole('ADMIN')" href="/admin">Admin</a>

    <!-- Raw HTML — explicit unsafe, tên rõ ràng -->
    <div kw:utext="${trustedHtmlContent}">content</div>

    <!-- Expression -->
    <p kw:text="'Hello, ' + user.name + '!'">Hello, Name!</p>
    <p kw:text="${items.size()} + ' items'">0 items</p>

    <!-- CSRF token tự động trong form POST -->
    <form method="POST" action="/profile">
        <!-- kernleaf injects hidden field: <input type="hidden" name="_csrf" value="..."> -->
        <button type="submit">Save</button>
    </form>
</body>
</html>
```

## Template Context

```rust
// Derive macro — compile-time field access, không dùng runtime reflection
#[derive(TemplateContext)]
struct ProfileContext {
    user: User,
    posts: Vec<Post>,
    is_admin: bool,
}

// Controller:
#[route(GET, "/profile/{id}")]
async fn profile(Path(id): Path<u64>, service: Arc<UserService>) -> impl IntoResponse {
    let user = service.find_by_id(id).await?;
    let posts = service.find_posts(id).await?;
    Template::new("profile/show", ProfileContext {
        is_admin: user.role == Role::Admin,
        posts,
        user,
    })
}
```

## Configuration

```rust
// 1 dòng để enable:
KernwayApp::builder()
    .plugin(KernleafPlugin::default())

// Custom config:
KernleafPlugin::builder()
    .template_dir("templates/")       // default
    .cache(true)                       // parse once, render many
    .strict_mode(true)                 // error on missing variable (production)
    .build()
```

## Security

| Feature | Default | Override |
|---|---|---|
| HTML escaping in `kw:text` | ✅ Enabled | `kw:utext` to disable it (explicit) |
| CSRF token in POST forms | ✅ Automatic | `.csrf(false)` to disable it |
| `kw:authorize` role check | ✅ Fail-closed | Renders empty if no principal is present |
| URL encoding in `kw:href` | ✅ Automatic | cannot be disabled |

## Natural Templates

HTML files can still be opened directly in the browser without a server — placeholder values are shown instead of expressions:

```html
<!-- Trong browser: "Placeholder Name" -->
<!-- Qua kernleaf: actual user name -->
<h1 kw:text="${user.name}">Placeholder Name</h1>
```

Designers can preview templates in the browser without running the server.
