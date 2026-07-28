//! Live gate for slice 3: resolve a real domain against a public resolver over
//! the async UDP path. `#[ignore]` so CI never depends on the network — run with
//! `cargo test -p kernway-dns --test real_dns -- --ignored`.

use kernway_dns::Resolver;
use rt_core::Executor;

#[test]
#[ignore = "hits the network; run with --ignored"]
fn resolves_example_com_via_cloudflare() {
    let ex = Executor::new().unwrap();
    let addrs = ex
        .block_on(async {
            let r = Resolver::new(vec!["1.1.1.1:53".parse().unwrap()]);
            r.lookup_a("example.com").await
        })
        .unwrap()
        .expect("example.com should resolve");
    assert!(!addrs.is_empty(), "expected at least one A record");
    println!("example.com A -> {addrs:?}");
}

#[test]
#[ignore = "hits the network; run with --ignored"]
fn resolves_ipv6_and_the_combined_lookup() {
    let ex = Executor::new().unwrap();
    ex.block_on(async {
        let r = Resolver::new(vec!["1.1.1.1:53".parse().unwrap()]);

        let v6 = r.lookup_aaaa("example.com").await.expect("AAAA lookup");
        assert!(v6.iter().all(|ip| ip.is_ipv6()), "AAAA must be IPv6: {v6:?}");
        assert!(!v6.is_empty(), "example.com has AAAA records");
        println!("example.com AAAA -> {v6:?}");

        let any = r.lookup("example.com").await.expect("combined lookup");
        assert!(!any.is_empty());
        println!("example.com lookup -> {any:?}");
    })
    .unwrap();
}
