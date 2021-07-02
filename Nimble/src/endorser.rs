mod endorser_state;
mod errors;
mod helper;

use crate::endorser_state::Store;
use crate::errors::EndorserError;
use endorser_proto::endorser_call_server::{EndorserCall, EndorserCallServer};
use endorser_proto::{
  Empty, EndorserAppendRequest, EndorserAppendResponse, EndorserLedgerResponse, EndorserPublicKey,
  EndorserQuery, EndorserQueryResponse, Handle,
};
use std::sync::{Arc, RwLock};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

pub mod endorser_proto {
  tonic::include_proto!("endorser_proto");
}

pub struct EndorserServiceState {
  state: Arc<RwLock<Store>>,
}

impl EndorserServiceState {
  pub fn new() -> Self {
    EndorserServiceState {
      state: Arc::new(RwLock::new(Store::new())),
    }
  }
}

#[tonic::async_trait]
impl EndorserCall for EndorserServiceState {
  async fn get_endorser_public_key(
    &self,
    _request: Request<Empty>,
  ) -> Result<Response<EndorserPublicKey>, Status> {
    let state_instance = self
      .state
      .read()
      .expect("Failed to acquire read lock")
      .get_endorser_key_information();

    if !state_instance.is_ok() {
      Err(EndorserError::InvalidLedgerName).unwrap()
    }
    let public_key = state_instance.unwrap();
    let reply = EndorserPublicKey {
      publickey: public_key.get_public_key(),
      signature: public_key.get_signature(),
    };

    Ok(Response::new(reply))
  }

  async fn new_ledger(
    &self,
    request: Request<Handle>,
  ) -> Result<Response<EndorserLedgerResponse>, Status> {
    // The handle is the byte array of information sent by the Nimble Coordinator to the Endorser
    let Handle { handle } = request.into_inner();

    let zero_entry = [0u8; 32].to_vec();
    let ledger_height = 0u64;
    let ledger_height_bytes = ledger_height.to_be_bytes().to_vec();
    let mut message: Vec<u8> = vec![];
    message.extend(zero_entry);
    message.extend(handle.to_vec());
    message.extend(ledger_height_bytes);

    let tail_hash = helper::hash(&message).to_vec();

    let mut state_instance = self
      .state
      .write()
      .expect("Unable to get a write lock on EndorserState");

    let signature = state_instance
      .create_new_ledger_in_endorser_state(handle, tail_hash, ledger_height)
      .expect("Unable to get the signature on genesis handle");

    let reply = EndorserLedgerResponse {
      signature: signature.to_bytes().to_vec(),
    };
    Ok(Response::new(reply))
  }

  async fn append_to_ledger(
    &self,
    request: Request<EndorserAppendRequest>,
  ) -> Result<Response<EndorserAppendResponse>, Status> {
    let EndorserAppendRequest {
      endorser_handle,
      block_hash,
      conditional_tail_hash,
    } = request.into_inner();
    let mut endorser_state = self.state.write().expect("Unable to obtain write lock");
    let append_status = endorser_state.append_and_update_endorser_state_tail(
      endorser_handle,
      block_hash,
      conditional_tail_hash,
    );

    if append_status.is_ok() {
      let (tail_hash, ledger_height, signature) = append_status.unwrap();
      let signature_bytes = signature.to_bytes().to_vec();
      let reply = EndorserAppendResponse {
        tail_hash,
        ledger_height,
        signature: signature_bytes,
      };
      return Ok(Response::new(reply));
    }
    Err(Status::aborted("Failed to Append"))
  }

  async fn read_latest(
    &self,
    request: Request<EndorserQuery>,
  ) -> Result<Response<EndorserQueryResponse>, Status> {
    let EndorserQuery { handle, nonce } = request.into_inner();
    let latest_state = self.state.read().expect("Failed to acquire read lock");
    let (nonce_bytes, tail_hash, endorser_signature) = latest_state
      .get_latest_state_for_handle(handle, nonce)
      .unwrap();
    let reply = EndorserQueryResponse {
      nonce: nonce_bytes,
      tail_hash,
      signature: endorser_signature.to_bytes().to_vec(),
    };
    Ok(Response::new(reply))
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  #[rustfmt::skip]
      let addr = "[::1]:9090".parse()?;
  let server = EndorserServiceState::new();

  println!("Running gRPC Endorser Service at {:?}", addr);

  Server::builder()
    .add_service(EndorserCallServer::new(server))
    .serve(addr)
    .await?;

  Ok(())
}
