use axum::extract::FromRef;
use axum::extract::State;
use axum::routing::post;
use axum::{extract::Path, http::StatusCode, routing::get, Router, Server};
use clap::Parser;
use file_lock::{FileLock, FileOptions};
use ic_config::execution_environment;
use ic_config::subnet_config::SubnetConfig;
use ic_crypto::threshold_sig_public_key_to_der;
use ic_crypto_iccsa::types::SignatureBytes;
use ic_crypto_iccsa::{public_key_bytes_from_der, verify};
use ic_crypto_utils_threshold_sig_der::parse_threshold_sig_key_from_der;
use ic_registry_subnet_type::SubnetType;
use ic_state_machine_tests::{StateMachine, StateMachineBuilder, StateMachineConfig};
use ic_types::{CanisterId, Cycles, PrincipalId};
use itertools::Itertools;
use pocket_ic::{CanisterCall, RawCanisterId, Request, Request::*};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::Duration;

const TTL_SEC: u64 = 30;

type InstanceId = String;
// The shared, mutable state of the PocketIC process.
// In essence, a Map<InstanceId, StateMachine>, but due to shared mutability, some extra layers are needed.
//
// The outer RwLock is for concurrent read access to the Map (such as calls to different instances),
// and exclusive write access (when a new instance is created or destroyed).
// The inner RwLock should allow safe concurrent calls to the same instance. TODO: Confirm this.
type InstanceMap = Arc<RwLock<HashMap<InstanceId, RwLock<StateMachine>>>>;

#[derive(Clone)]
struct AppState {
    instance_map: InstanceMap,
    last_request: Arc<RwLock<Instant>>,
}

impl FromRef<AppState> for InstanceMap {
    fn from_ref(app_state: &AppState) -> InstanceMap {
        app_state.instance_map.clone()
    }
}

impl FromRef<AppState> for Arc<RwLock<Instant>> {
    fn from_ref(app_state: &AppState) -> Arc<RwLock<Instant>> {
        app_state.last_request.clone()
    }
}

// Command line arguments to PocketIC service
#[derive(Parser)]
struct Args {
    #[clap(long)]
    lock_file: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let file_opts = FileOptions::new().write(true).create(true);
    let mut lock_file = FileLock::lock(args.lock_file, false, file_opts)
        .expect("Failed to acquire lock on port file. Shutting down.");

    // The shared, mutable state of the PocketIC process.
    let instance_map: InstanceMap = Arc::new(RwLock::new(HashMap::new()));
    // A time-to-live mechanism: Requests bump this value, and the server
    // gracefully shuts down when the value wasn't bumped for a while
    // TODO: Implement ttl increase for every handler.
    let last_request = Arc::new(RwLock::new(Instant::now()));
    let app_state = AppState {
        instance_map,
        last_request,
    };

    let app = Router::new()
        //
        // Get health of service.
        .route("/status", get(status))
        //
        // List all IC instances.
        .route("/instance", get(list_instances))
        //
        // Create a new IC instance. Returns an InstanceId.
        // Body is currently ignored.
        .route("/instance", post(create_instance))
        //
        // Call the specified IC instance.
        // Body contains a Request.
        // Returns the IC's Response.
        .route("/instance/:id", post(call_instance))
        .with_state(app_state.clone());

    // bind to port 0; the OS will give a specific port; communicate that to parent process via stdout
    let server = Server::bind(&"127.0.0.1:0".parse().expect("Failed to parse address"))
        .serve(app.into_make_service());
    let real_port = server.local_addr().port();
    let _ = lock_file.file.write_all(real_port.to_string().as_bytes());
    let _ = lock_file.file.flush();

    // This is a safeguard against orphaning this child process.
    let shutdown_signal = async {
        loop {
            let guard = app_state.last_request.read().await;
            // TODO: implement ttl increase for every handler.
            if guard.elapsed() > Duration::from_secs(TTL_SEC) {
                break;
            }
            drop(guard);
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        eprintln!("PocketIC process will exit.");
    };
    let server = server.with_graceful_shutdown(shutdown_signal);
    server.await.expect("Failed to launch pocketIC.");
}

async fn status() -> StatusCode {
    StatusCode::OK
}

/// Create a new IC instance.
/// The new InstanceId will be returned
async fn create_instance(State(inst_map): State<InstanceMap>) -> String {
    let instance_id = rand_string(6);
    let sm = tokio::task::spawn_blocking(|| create_state_machine())
        .await
        .expect("Failed to launch a state machine");
    let mut guard = inst_map.write().await;
    guard.insert(instance_id.clone(), RwLock::new(sm));
    instance_id
}

async fn list_instances(State(inst_map): State<InstanceMap>) -> String {
    let map_guard = inst_map.read().await;
    map_guard.keys().join(", ")
}

// Call the IC instance with the given InstanceId
async fn call_instance(
    State(inst_map): State<InstanceMap>,
    Path(id): Path<InstanceId>,
    axum::extract::Json(request): axum::extract::Json<Request>,
) -> String {
    // println!("call_instance {} with request: {}", id, serde_json::to_string(&request).unwrap_or("Failed to decode json".to_owned()));
    let guard_map = inst_map.read().await;
    if let Some(rw_lock) = guard_map.get(&id) {
        let guard_sm = rw_lock.write().await;
        call_sm(&guard_sm, request)
    } else {
        // id not found in map; return error
        // TODO: Result Type for this call
        format!("Id {} was not found in instance map.", id)
    }
}

fn create_state_machine() -> StateMachine {
    let hypervisor_config = execution_environment::Config {
        default_provisional_cycles_balance: Cycles::new(0),
        ..Default::default()
    };
    let config = StateMachineConfig::new(SubnetConfig::new(SubnetType::System), hypervisor_config);
    StateMachineBuilder::new().with_config(Some(config)).build()
}

fn rand_string(len: usize) -> String {
    use rand::distributions::Alphanumeric;
    use rand::thread_rng;
    use rand::Rng;
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

// ===================================================================================
// Code borrowed from rs/state_machine_tests/src/main.rs

fn call_sm(sm: &StateMachine, data: Request) -> String {
    match data {
        RootKey => to_json_str(threshold_sig_public_key_to_der(sm.root_key()).unwrap()),
        Time => to_json_str(sm.time()),
        SetTime(time) => {
            sm.set_time(time);
            to_json_str(())
        }
        AdvanceTime(amount) => {
            sm.advance_time(amount);
            to_json_str(())
        }
        CanisterUpdateCall(call) => {
            let mut call = ParsedCanisterCall::from(call);
            if call.canister_id == CanisterId::ic_00() && call.method == "create_canister" {
                call.method = "provisional_create_canister_with_cycles".to_string();
            }
            let result =
                sm.execute_ingress_as(call.sender, call.canister_id, call.method, call.arg);
            to_json_str(result)
        }
        CanisterQueryCall(call) => {
            let call = ParsedCanisterCall::from(call);
            let result = sm.query_as(call.sender, call.canister_id, call.method, call.arg);
            to_json_str(result)
        }
        CanisterExists(canister_id) => to_json_str(sm.canister_exists(to_canister_id(canister_id))),
        SetStableMemory(arg) => {
            let canister_id = CanisterId::try_from(arg.canister_id).expect("invalid canister id");
            sm.set_stable_memory(canister_id, arg.data.as_ref());
            to_json_str(())
        }
        ReadStableMemory(canister_id) => to_json_str(sm.stable_memory(to_canister_id(canister_id))),
        CyclesBalance(canister_id) => to_json_str(sm.cycle_balance(to_canister_id(canister_id))),
        AddCycles(arg) => to_json_str(sm.add_cycles(
            CanisterId::try_from(arg.canister_id).expect("invalid canister id"),
            arg.amount,
        )),
        Tick => {
            sm.tick();
            to_json_str(())
        }
        RunUntilCompletion(arg) => {
            sm.run_until_completion(arg.max_ticks as usize);
            to_json_str(())
        }
        VerifyCanisterSig(arg) => {
            type VerificationResult = Result<(), String>;
            let pubkey = match public_key_bytes_from_der(&arg.pubkey) {
                Ok(pubkey) => pubkey,
                Err(err) => {
                    return to_json_str(VerificationResult::Err(format!(
                        "failed to parse DER encoded public key: {:?}",
                        err
                    )));
                }
            };
            let root_pubkey = match parse_threshold_sig_key_from_der(&arg.root_pubkey) {
                Ok(root_pubkey) => root_pubkey,
                Err(err) => {
                    return to_json_str(VerificationResult::Err(format!(
                        "failed to parse DER encoded root public key: {:?}",
                        err
                    )));
                }
            };
            match verify(&arg.msg, SignatureBytes(arg.sig), pubkey, &root_pubkey) {
                Ok(()) => to_json_str(VerificationResult::Ok(())),
                Err(err) => to_json_str(VerificationResult::Err(format!(
                    "canister signature verification failed: {:?}",
                    err
                ))),
            }
        }
    }
}

fn to_json_str<R: Serialize>(response: R) -> String {
    serde_json::to_string(&response).expect("Failed to serialize to json")
}

fn to_canister_id(raw_id: RawCanisterId) -> CanisterId {
    CanisterId::try_from(raw_id.canister_id).expect("invalid canister id")
}

struct ParsedCanisterCall {
    sender: PrincipalId,
    canister_id: CanisterId,
    method: String,
    arg: Vec<u8>,
}

impl From<CanisterCall> for ParsedCanisterCall {
    fn from(call: CanisterCall) -> Self {
        ParsedCanisterCall {
            sender: PrincipalId::try_from(&call.sender).unwrap_or_else(|err| {
                panic!(
                    "failed to parse sender from bytes {}: {}",
                    hex::encode(&call.sender),
                    err
                )
            }),
            canister_id: CanisterId::try_from(&call.canister_id).unwrap_or_else(|err| {
                panic!(
                    "failed to parse canister id from bytes {}: {}",
                    hex::encode(&call.canister_id),
                    err
                )
            }),
            method: call.method,
            arg: call.arg,
        }
    }
}
