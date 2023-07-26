use candid::{encode_one, Principal};
use once_cell::sync::Lazy;
use pocket_ic::{PocketICService, WasmResult, IC};
use std::path::PathBuf;

// temporary way to tie the lifetime of the backend server process
// to the process which runs these tests.
static PIC: Lazy<PocketICService> =
    Lazy::new(|| PocketICService::new(PathBuf::from("../../target/debug/pocket-ic-backend")));

// tests in one file may run concurrently
// test sets from different files run in sequence
#[test]
fn test_1() {
    println!("===== Test 1 start =====");
    test_counter_canister();
    println!("===== Test 1 end   =====");
}

#[test]
fn test_2() {
    println!("===== Test 2 start =====");
    test_counter_canister();
    println!("===== Test 2 end   =====");
}

fn test_counter_canister() {
    let counter_wasm = std::fs::read("./tests/counter.wasm").expect("Failed to load counter.wasm.");
    let ic: IC = PIC.create_ic();
    println!("all instances: {:?}", PIC.list_instances());

    let controller = Principal::anonymous();
    let can_id = ic.create_canister(Some(controller));
    println!("canister id: {}", can_id);
    ic.add_cycles(can_id, 1_000_000_000_000_000_000);
    ic.install_canister(can_id, counter_wasm, vec![], Some(controller));

    let reply = call_counter_can(&ic, can_id, controller, "read");
    println!("{:?}", reply);
    assert!(reply == WasmResult::Reply(vec![0, 0, 0, 0]));

    let reply = call_counter_can(&ic, can_id, controller, "write");
    println!("{:?}", reply);
    assert!(reply == WasmResult::Reply(vec![1, 0, 0, 0]));

    let reply = call_counter_can(&ic, can_id, controller, "write");
    println!("{:?}", reply);
    assert!(reply == WasmResult::Reply(vec![2, 0, 0, 0]));

    let reply = call_counter_can(&ic, can_id, controller, "read");
    println!("{:?}", reply);
    assert!(reply == WasmResult::Reply(vec![2, 0, 0, 0]));
}

fn call_counter_can(ic: &IC, can_id: Principal, sender: Principal, method: &str) -> WasmResult {
    ic.update_call(can_id, sender, method, encode_one(()).unwrap())
        .expect("Failed to call counter canister")
}
