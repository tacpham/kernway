#![allow(missing_docs)] // a benchmark binary, not public API
//! kernleaf vs minijinja — the head-to-head that decides whether the hand-written
//! engine earns its place (KEP-0000 §2), and shows that syntax is not a speed
//! question.
//!
//! kernleaf speaks Thymeleaf (`th:*` attributes), minijinja speaks Jinja — two
//! different surfaces that here compile to the **same HTML output**, which the
//! bench asserts before timing. So it is the same work both sides, escaped both
//! sides, and the only difference is the engine. `render` is the hot path;
//! parsing happens once, off the request path, and is measured separately in
//! `parse` to show it is comparable but irrelevant per request.
//!
//! Run: `cargo bench -p kernleaf`

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use kernleaf::Kernleaf;
use kernway_core::template::{TemplateEngine, Value};
use minijinja::{context, Environment, Value as MjValue};

// Same output, two syntaxes: a title plus a 50-row list of escaped names.
const KL_TMPL: &str = "<h1 th:text=\"${title}\">t</h1><ul>\
<li th:each=\"u : ${users}\" th:text=\"${u.name}\">n</li>\
</ul>";
const MJ_TMPL: &str = "<h1>{{ title }}</h1><ul>\
{% for u in users %}<li>{{ u.name }}</li>{% endfor %}\
</ul>";

const N: usize = 50;

fn kernleaf_model() -> Value<'static> {
    let users = (0..N)
        .map(|i| Value::map([("name", Value::from(format!("User {i}")))]))
        .collect::<Vec<_>>();
    Value::map([("title", Value::from("Team")), ("users", Value::Seq(users))])
}

fn minijinja_model() -> MjValue {
    let users: Vec<MjValue> = (0..N)
        .map(|i| context! { name => format!("User {i}") })
        .collect();
    context! { title => "Team", users => users }
}

fn render(c: &mut Criterion) {
    let mut g = c.benchmark_group("render/user_list_50");

    let mut kl = Kernleaf::new();
    kl.add("users", KL_TMPL).unwrap();
    let km = kernleaf_model();
    // Sanity: both engines produce identical, escaped output before we time them.
    let kl_out = kl.render("users", &km).unwrap();

    let mut env = Environment::new();
    env.add_template("users.html", MJ_TMPL).unwrap(); // .html → minijinja auto-escapes, matching kernleaf
    let tmpl = env.get_template("users.html").unwrap();
    let mm = minijinja_model();
    let mj_out = tmpl.render(&mm).unwrap();
    assert_eq!(
        kl_out, mj_out,
        "engines must render identical output to compare fairly"
    );

    g.bench_function("kernleaf", |b| {
        b.iter(|| black_box(kl.render("users", black_box(&km)).unwrap()))
    });
    g.bench_function("minijinja", |b| {
        b.iter(|| black_box(tmpl.render(black_box(&mm)).unwrap()))
    });

    g.finish();
}

fn parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse/user_list");

    let mut kl = Kernleaf::new();
    g.bench_function("kernleaf", |b| {
        b.iter(|| kl.add("t", black_box(KL_TMPL)).unwrap())
    });

    let mut env = Environment::new();
    g.bench_function("minijinja", |b| {
        b.iter(|| env.add_template("t", black_box(MJ_TMPL)).unwrap())
    });

    g.finish();
}

criterion_group!(benches, render, parse);
criterion_main!(benches);
