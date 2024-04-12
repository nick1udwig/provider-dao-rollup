use serde::{Deserialize, Serialize};

use kinode_process_lib::{
    await_next_message_body, call_init, println, Address, LazyLoadBlob, Request,
};

mod engine;
use engine::DaoTransaction;
mod rollup_lib;
use rollup_lib::SignedTransaction;

wit_bindgen::generate!({
    path: "wit",
    world: "process",
    exports: {
        world: Component,
    },
});

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SequencerRequest {
    Read(ReadRequest),
    Write(SignedTransaction<engine::DaoState>),
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
    All(engine::DaoState),
    Dao,
    Routers(Vec<String>),  // length 1 for now
    Members(Vec<String>),  // TODO: should probably be the HashMap
    Proposals,
    Parameters,
}

const PUBLISHER: &str = "nick1udwig.os";
const PROCESS_NAME: &str = "sequencer";
const SCRIPT_NAME: &str = "read";

call_init!(init);
fn init(our: Address) {
    let Ok(body) = await_next_message_body() else {
        println!("failed to get args!");
        return;
    };

    let package_name = our.package();

    let body = String::from_utf8(body)
        .expect(&format!("usage:\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} node read_request\ne.g.\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} our \"Routers\""));
    let (node, request) = body.split_once(" ")
        .expect(&format!("usage:\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} node read_request\ne.g.\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} our \"Routers\""));
    let node = if node != "our" { node } else { our.node() };

    let request: ReadRequest = match serde_json::from_str(request) {
        Ok(rr) => rr,
        Err(_e) => {
            println!("usage:\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} node read_request\ne.g.\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} our \"Routers\"");
            return;
        },
    };

    let Ok(Ok(response)) =
        Request::to((node, (PROCESS_NAME, package_name, PUBLISHER)))
            .body(vec![])
            .blob_bytes(serde_json::to_vec(&SequencerRequest::Read(request)).unwrap())
            .send_and_await_response(5)
    else {
        println!("did not receive Response from {PROCESS_NAME}:{package_name}:{PUBLISHER}");
        return;
    };
    let Some(LazyLoadBlob { bytes, ..}) = response.blob() else {
        println!("did not receive Response with blob from {PROCESS_NAME}:{package_name}:{PUBLISHER}");
        return;
    };

    match serde_json::from_slice::<SequencerResponse>(&bytes) {
        Ok(response) => println!("SequencerResponse: {response:?}"),
        Err(err) => println!("did not receive Response of type ReadResponse from {PROCESS_NAME}:{package_name}:{PUBLISHER}; error: {err:?}"),
    }
}
