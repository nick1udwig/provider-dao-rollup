use serde::{Deserialize, Serialize};

use kinode_process_lib::{
    await_next_message_body, call_init, println, Address, Request,
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
enum AdminAction {
    Prove,
    BatchWithdrawals,
    SetRouters(Vec<String>),
    SetMembers(Vec<String>),
}

const PUBLISHER: &str = "nick1udwig.os";
const PROCESS_NAME: &str = "sequencer";
const SCRIPT_NAME: &str = "admin";

call_init!(init);
fn init(our: Address) {
    let Ok(body) = await_next_message_body() else {
        println!("failed to get args!");
        return;
    };

    let package_name = our.package();

    let request: AdminAction = match serde_json::from_slice(body.as_slice()) {
        Ok(rr) => rr,
        Err(_e) => {
            println!("usage:\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} admin_action\ne.g.\n{SCRIPT_NAME}:{package_name}:{PUBLISHER} {{\"SetRouters\": [\"{PUBLISHER}\"]}}");
            return;
        },
    };

    Request::to((our.node(), (PROCESS_NAME, package_name, PUBLISHER)))
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .unwrap_or_else(|e| println!("{}: failed to send: {e}", our.process()));
    // TODO
    //let Ok(Ok(response)) =
    //    Request::to((our.node(), (PROCESS_NAME, package_name, PUBLISHER)))
    //        .body(serde_json::to_vec(&request).unwrap())
    //        .send_and_await_response(5)
    //else {
    //    println!("did not receive Response from {PROCESS_NAME}:{package_name}:{PUBLISHER}");
    //    return;
    //};
    //let Some(response) = response.body() else {
    //    println!("did not receive Response with blob from {PROCESS_NAME}:{package_name}:{PUBLISHER}");
    //    return;
    //};

    //match serde_json::from_slice::<ReadResponse>(&bytes) {
    //    Ok(response) => println!("ReadResponse: {response:?}"),
    //    Err(err) => println!("did not receive Response of type AdminAction from {PROCESS_NAME}:{package_name}:{PUBLISHER}; error: {err:?}"),
    //}
}
