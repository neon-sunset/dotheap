use heap_core::{
    connections::{endpoint_address, ipv6_endpoint},
    tasks::{async_method, field_name},
};

#[test]
fn compiler_generated_names_keep_generic_types_and_local_functions() {
    for (machine, owner, method) in [
        ("Shop.Worker+<RunAsync>d__3", "Shop.Worker", "RunAsync"),
        (
            "Shop.Worker`1+<RunAsync>d__3<System.String>",
            "Shop.Worker`1",
            "RunAsync",
        ),
        (
            "Shop.Worker+<>c__DisplayClass0_0+<<Outer>g__Inner|1>d",
            "Shop.Worker",
            "Outer.Inner",
        ),
        (
            "Shop.Worker+<>c+<<Outer>b__1>d",
            "Shop.Worker",
            "Outer (lambda)",
        ),
        ("Shop.Работник+<Запуск>d__1", "Shop.Работник", "Запуск"),
    ] {
        let name = format!(
            "System.Runtime.CompilerServices.AsyncTaskMethodBuilder`1+AsyncStateMachineBox`1<System.Collections.Generic.Dictionary`2<System.String, System.Int32>, {machine}>"
        );
        assert_eq!(async_method(&name), (owner.into(), method.into()));
    }
    assert_eq!(field_name("<connection>5__2"), Some("connection".into()));
    assert_eq!(field_name("<>u__10"), Some("awaiter 10".into()));
    assert_eq!(field_name("<>t__builder"), None);
}

#[test]
fn socket_endpoints_preserve_ipv6_scope_and_unix_paths() {
    assert_eq!(
        ipv6_endpoint([0, 0, 0, 0, 0, 0xffff, 0xc000, 0x201], 0, 443),
        "192.0.2.1:443"
    );
    assert_eq!(
        ipv6_endpoint([0xfe80, 0, 0, 0, 0, 0, 0, 1], 3, 80),
        "[fe80::1%3]:80"
    );
    assert_eq!(
        ipv6_endpoint([0x2001, 0xdb8, 0, 0, 1, 0, 0, 1], 0, 80),
        "[2001:db8::1:0:0:1]:80"
    );
    assert_eq!(endpoint_address("[fe80::1%3]:80"), "[fe80::1%3]");
    assert_eq!(endpoint_address("/run/app:socket"), "/run/app:socket");
}
