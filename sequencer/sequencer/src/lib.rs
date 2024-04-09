#![feature(let_chains)]
use std::collections::HashMap;

use kinode_process_lib::eth;
use kinode_process_lib::kernel_types::MessageType;
use kinode_process_lib::{
    await_message, call_init, get_blob, http, println,
    vfs::{create_drive, create_file},
    Address, Message, Request, Response,
};
use serde::{Deserialize, Serialize};
use sp1_core::SP1Stdin;

mod bridge_lib;
use bridge_lib::{get_old_logs, handle_log, subscribe_to_logs};
mod engine;
use engine::{DaoTransaction, FullRollupState};
mod prover_types;
use prover_types::ProveRequest;
mod rollup_lib;
use rollup_lib::*;

const ELF: &[u8] = include_bytes!("../../../elf_program/elf/riscv32im-succinct-zkvm-elf");

#[derive(Debug, Clone, Serialize, Deserialize)]
enum AdminActions {
    Prove,
    BatchWithdrawals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SequencerRequest {
    Read(ReadRequest),
    Write(SignedTransaction<DaoTransaction>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SequencerResponse {
    Read(ReadResponse),
    Write,  // TODO: return hash of tx?
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ReadRequest {
    All,
    Dao,
    Routers,
    Members,
    Proposals,
    Parameters,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ReadResponse {
    All,
    Dao,
    Routers(Vec<String>),  // length 1 for now
    Members,
    Proposals,
    Parameters,
}

// Boilerplate: generate the wasm bindings for a process
wit_bindgen::generate!({
    path: "wit",
    world: "process",
    exports: {
        world: Component,
    },
});

// After generating bindings, use this macro to define the Component struct
// and its init() function, which the kernel will look for on startup.
call_init!(initialize);

fn initialize(our: Address) {
    // A little printout to show in terminal that the process has started.
    println!("{}: started", our.package());

    // Serve the index.html and other UI files found in pkg/ui at the root path.
    // authenticated=true, local_only=false
    http::serve_ui(&our, "ui", false, false, vec!["/"]).unwrap();
    // Allow HTTP requests to be made to /rpc; they will be handled dynamically.
    // The handling logic for this is the RPC API for reads and writes to the rollup.
    http::bind_http_path("/rpc", true, false).unwrap();
    // Allow websockets to be opened at / (our process ID will be prepended).
    // This lets the optional prover_extension connect to us.
    http::bind_ext_path("/").unwrap();

    // Grab our state
    let mut state = FullRollupState::load();

    // create a new eth provider to read logs from chain (deposits and state root updates)
    let eth_provider = eth::Provider::new(10, 5);

    // index all old deposits
    get_old_logs(&eth_provider, &mut state);
    state.save().unwrap();
    // subscribe to new deposits
    subscribe_to_logs(&eth_provider, state.l1_block);

    // enter the main event loop
    main_loop(&our, &mut state, &mut None);
}

fn main_loop(our: &Address, state: &mut FullRollupState, connection: &mut Option<u32>) {
    loop {
        // Call await_message() to wait for any incoming messages.
        // If we get a network error, make a print and throw it away.
        match await_message() {
            Err(send_error) => {
                // TODO: surface this to the user
                println!("{our}: got network error: {send_error:?}");
                continue;
            }
            Ok(message) => match handle_message(&our, &message, state, connection) {
                Ok(()) => continue,
                Err(e) => println!("{our}: error handling request: {:?}", e),
            },
        }
    }
}

/// Handle sequencer messages from ourself *or* other nodes.
fn handle_message(
    our: &Address,
    message: &Message,
    state: &mut FullRollupState,
    connection: &mut Option<u32>,
) -> anyhow::Result<()> {
    // no responses
    if !message.is_request() {
        return Ok(());
    }
    if message.source().node != our.node {
        // // got cross rollup message, implementation is TODO. Need to:
        // // - first verify that this message was posted to DA
        // // - then sequence it
        // return Ok(());
        let Some(blob) = get_blob() else {
            return Err(anyhow::anyhow!(
                "got Request without blob: expected Read or Write() in blob"
            ));
        };
        match serde_json::from_slice::<SequencerRequest>(&blob.bytes)? {
            SequencerRequest::Read(read_type) => {
                match read_type {
                    ReadRequest::Routers => {
                        Response::new()
                            .blob_bytes(serde_json::to_vec(&ReadResponse::Routers(
                                state.state.routers.clone()
                            ))?)
                            .send()?;
                    }
                    _ => unimplemented!("only Routers is currently implemented"),  // TODO
                }
            }
            SequencerRequest::Write(tx) => {
                // get the blob from the request
                let Some(blob) = get_blob() else {
                    return Ok(http::send_response(
                        http::StatusCode::BAD_REQUEST,
                        None,
                        vec![],
                    ));
                };
                // deserialize the blob into a SignedTransaction
                let tx =
                    serde_json::from_slice::<SignedTransaction<DaoTransaction>>(&blob.bytes)?;

                // execute the transaction, which will propagate any errors like a bad signature or bad move
                let node = Some(message.source().node.clone());
                state.execute(tx.clone(), node.clone())?;
                // save the tx for proving
                state.sequenced.push((tx, node));
                state.save()?;
                // send a confirmation to the requestor that the transaction was sequenced
                Response::new()
                    .blob_bytes(vec![])
                    .send()?;
            }
        }
        return Ok(())
    }
    return match message.source().process.to_string().as_str() {
        "http_server:distro:sys" => {
            return handle_http_request(our, state, connection, message);
        }
        "eth:distro:sys" => {
            // we need to first extract the log
            let Ok(Ok(eth::EthSub { result, .. })) =
                serde_json::from_slice::<eth::EthSubResult>(message.body())
            else {
                return Err(anyhow::anyhow!("sequencer: got invalid message"));
            };
            let eth::SubscriptionResult::Log(log) = result else {
                panic!("expected log");
            };
            // then we handle the log with the standard bridge_lib::handle_log
            // which implements the default, audited way to interact with deposits
            handle_log(state, &log)
        }
        _ => handle_admin_message(&our, message, state, connection),
    };
}

/// Handle HTTP requests from our own frontend.
fn handle_http_request(
    our: &Address,
    state: &mut FullRollupState,
    connection: &mut Option<u32>,
    message: &Message,
) -> anyhow::Result<()> {
    match serde_json::from_slice::<http::HttpServerRequest>(message.body())? {
        // GETs and POSTs are reads and writes to the chain, respectively
        // essentially, this is our RPC API
        http::HttpServerRequest::Http(ref incoming) => {
            match incoming.method()?.as_str() {
                // TODO:
                //  in-Kinode messaging
                //  * queries of state (e.g., for fetching root node)
                //  * writes to state (e.g., for proposing/voting)
                // chain reads
                "GET" => {
                    // TODO: different paths
                    http::send_response(
                        http::StatusCode::OK,
                        Some(HashMap::from([(
                            String::from("Content-Type"),
                            String::from("application/json"),
                        )])),
                        serde_json::to_vec(&ReadResponse::Routers(state.state.routers.clone()))?,
                    );
                }
                // chain writes (handle transactions)
                "POST" => {
                    // get the blob from the request
                    let Some(blob) = get_blob() else {
                        return Ok(http::send_response(
                            http::StatusCode::BAD_REQUEST,
                            None,
                            vec![],
                        ));
                    };
                    // deserialize the blob into a SignedTransaction
                    let tx =
                        serde_json::from_slice::<SignedTransaction<DaoTransaction>>(&blob.bytes)?;

                    // execute the transaction, which will propagate any errors like a bad signature or bad move
                    let node = None;
                    state.execute(tx.clone(), node.clone())?;
                    // save the tx for proving
                    state.sequenced.push((tx, node));
                    state.save()?;
                    // send a confirmation to the frontend that the transaction was sequenced
                    http::send_response(
                        http::StatusCode::OK,
                        None,
                        "send tx receipt or error here" // TODO better receipt
                            .to_string()
                            .as_bytes()
                            .to_vec(),
                    );
                }
                // Any other http method will be rejected
                // feel free to add more methods if you need them
                _ => {
                    http::send_response(
                        http::StatusCode::METHOD_NOT_ALLOWED,
                        None,
                        vec![],
                    );
                }
            }
            Ok(())
        }
        // this is for connecting to the prover_extension
        http::HttpServerRequest::WebSocketOpen { ref channel_id, .. } => {
            println!("sequencer: connected to prover_extension");
            *connection = Some(*channel_id);
            Ok(())
        }
        // this is for disconnecting to the prover_extension
        http::HttpServerRequest::WebSocketClose(ref channel_id) => {
            if connection.unwrap_or(0) != *channel_id {
                return Err(anyhow::anyhow!("WebSocketClose wrong channel_id"));
            }
            println!("sequencer: dropped connection with prover_extension");
            *connection = None;
            Ok(())
        }
        // this is for receiving resopnses to when we receive a proof back (see below for the request)
        http::HttpServerRequest::WebSocketPush {
            ref channel_id,
            ref message_type,
        } => {
            // if the channel_id is wrong, then we don't want to accept any messages
            if connection.unwrap_or(0) != *channel_id {
                return Err(anyhow::anyhow!("wrong channel_id"));
            }
            let http::WsMessageType::Binary = message_type else {
                return Err(anyhow::anyhow!("expected binary message"));
            };

            let Some(blob) = get_blob() else {
                return Err(anyhow::anyhow!("WebSocketPush Binary had no blob"));
            };

            let kinode_process_lib::http::HttpServerAction::WebSocketExtPushData { blob, .. } =
                serde_json::from_slice(&blob.bytes)?
            else {
                return Err(anyhow::anyhow!("expected WebSocketExtPushData"));
            };
            // save the proof to the vfs
            let drive_path: String = create_drive(our.package_id(), "proofs", Some(5))?;
            let proof_file = create_file(&format!("{}/proof.json", &drive_path), Some(5))?;
            proof_file.write(&blob)?;
            // TODO post this proof to the L1 verifier
            Ok(())
        }
    }
}

// the only admin action we have is to prove the current state, more to come in the future
fn handle_admin_message(
    our: &Address,
    message: &Message,
    state: &mut FullRollupState,
    connection: &mut Option<u32>,
) -> anyhow::Result<()> {
    match serde_json::from_slice::<AdminActions>(message.body())? {
        AdminActions::Prove => {
            let Some(channel_id) = connection else {
                return Err(anyhow::anyhow!("no connection"));
            };

            let mut stdin = SP1Stdin::new();
            stdin.write(&state.sequenced.clone());

            // send a request to the prover_extension to prove the current state
            Request::new()
                .target("our@http_server:distro:sys".parse::<Address>()?)
                .body(serde_json::to_vec(
                    &http::HttpServerAction::WebSocketExtPushOutgoing {
                        channel_id: *channel_id,
                        message_type: http::WsMessageType::Binary,
                        desired_reply_type: MessageType::Response,
                    },
                )?)
                .blob_bytes(bincode::serialize(&ProveRequest {
                    elf: ELF.to_vec(),
                    input: stdin,
                })?)
                .send()
                .unwrap();

            // TODO: is this correct?
            state.sequenced = vec![];

            // NOTE the response comes in as a WebSocketPush message, see above
            Ok(())
        }
        AdminActions::BatchWithdrawals => {
            let batch = WithdrawTree::new(state.withdrawals.clone());
            state.batches.push(batch);

            // backup the withdrawal tree to vfs
            let drive_path: String = create_drive(our.package_id(), "batches", Some(5))?;
            let withdrawal_file = create_file(
                &format!("{}/{}.json", &drive_path, &state.batches.len()),
                Some(5),
            )?;
            withdrawal_file.write(&serde_json::to_vec(state.batches.last().unwrap()).unwrap())?;

            state.withdrawals = vec![];

            Ok(())
        }
    }
}
