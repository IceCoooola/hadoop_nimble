use crate::errors::CoordinatorError;
use ledger::{
  signature::{PublicKey, PublicKeyTrait},
  Handle, NimbleDigest, Nonce, Receipt,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tonic::transport::{Channel, Endpoint};

pub mod endorser_proto {
  tonic::include_proto!("endorser_proto");
}

use endorser_proto::endorser_call_client::EndorserCallClient;
use endorser_proto::{
  AppendReq, AppendResp, AppendViewLedgerReq, AppendViewLedgerResp, GetPublicKeyReq,
  GetPublicKeyResp, InitializeStateReq, InitializeStateResp, LedgerTailMapEntry, NewLedgerReq,
  NewLedgerResp, ReadLatestReq, ReadLatestResp, ReadLatestViewLedgerReq, ReadLatestViewLedgerResp,
};

#[derive(Debug, Default)]
pub struct ConnectionStore {
  store: Arc<RwLock<HashMap<Vec<u8>, EndorserCallClient<Channel>>>>,
}

impl ConnectionStore {
  pub fn new() -> ConnectionStore {
    ConnectionStore {
      store: Arc::new(RwLock::new(HashMap::new())),
    }
  }

  pub fn get_all(&self) -> Vec<Vec<u8>> {
    self
      .store
      .read()
      .expect("Failed to get the read lock")
      .iter()
      .map(|(pk, _ec)| pk.clone())
      .collect::<Vec<Vec<u8>>>()
  }

  pub async fn connect_endorser(&mut self, hostname: String) -> Result<Vec<u8>, CoordinatorError> {
    let res = Endpoint::from_shared(hostname.to_string());
    if res.is_err() {
      return Err(CoordinatorError::CannotResolveHostName);
    }
    let endorser_endpoint = res.unwrap();
    let channel = endorser_endpoint.connect_lazy();
    let mut client = EndorserCallClient::new(channel);

    let req = tonic::Request::new(GetPublicKeyReq {});
    let res = client.get_public_key(req).await;
    if res.is_err() {
      return Err(CoordinatorError::FailedToConnectToEndorser);
    }
    let GetPublicKeyResp { pk } = res.unwrap().into_inner();
    println!("Connected Successfully to {:?}", &hostname);

    let res = PublicKey::from_bytes(&pk);
    if res.is_err() {
      return Err(CoordinatorError::UnableToRetrievePublicKey);
    }

    if let Ok(mut conn_map) = self.store.write() {
      conn_map.insert(pk.clone(), client);
    } else {
      eprintln!("Failed to acquire the write lock");
      return Err(CoordinatorError::FailedToAcquireWriteLock);
    }
    Ok(pk)
  }

  pub async fn initialize_state(
    &mut self,
    endorsers: &[Vec<u8>],
    ledger_tail_map: &HashMap<NimbleDigest, (NimbleDigest, usize)>,
    view_ledger_tail_height: &(NimbleDigest, usize),
    block_hash: &NimbleDigest,
    cond_updated_tail_hash: &NimbleDigest,
  ) -> Result<Receipt, CoordinatorError> {
    let ledger_tail_map_proto: Vec<LedgerTailMapEntry> = ledger_tail_map
      .iter()
      .map(|(handle, (tail, height))| LedgerTailMapEntry {
        handle: handle.to_bytes(),
        tail: tail.to_bytes(),
        height: *height as u64,
      })
      .collect();

    let mut jobs = Vec::new();
    if let Ok(conn_map) = self.store.read() {
      for pk in endorsers {
        if !conn_map.contains_key(pk) {
          eprintln!("No endorser has this public key {:?}", pk);
          return Err(CoordinatorError::InvalidEndorserPublicKey);
        }
        let mut endorser_client = conn_map[pk].clone();
        let ledger_tail_map = ledger_tail_map_proto.clone();
        let view_ledger_tail = view_ledger_tail_height.0.to_bytes();
        let view_ledger_height = view_ledger_tail_height.1 as u64;
        let block_hash = block_hash.to_bytes();
        let cond_updated_tail_hash = cond_updated_tail_hash.to_bytes();
        let pk_bytes = pk.clone();
        let job = tokio::spawn(async move {
          let response = endorser_client
            .initialize_state(tonic::Request::new(InitializeStateReq {
              ledger_tail_map,
              view_ledger_tail,
              view_ledger_height,
              block_hash,
              cond_updated_tail_hash,
            }))
            .await;
          (pk_bytes, response)
        });
        jobs.push(job);
      }
    } else {
      eprintln!("Failed to acquire the read lock");
      return Err(CoordinatorError::FailedToAcquireReadLock);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let InitializeStateResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("initialize_state failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToInitializeEndorser);
        }
      } else {
        eprintln!("initialize_state failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToInitializeEndorser);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }

  pub async fn create_ledger(&self, ledger_handle: &Handle) -> Result<Receipt, CoordinatorError> {
    let mut jobs = Vec::new();
    for (pk, ec) in self
      .store
      .read()
      .expect("Failed to get the read lock")
      .iter()
    {
      let mut endorser_client = ec.clone();
      let handle = *ledger_handle;
      let pk_bytes = pk.clone();
      let job = tokio::spawn(async move {
        let response = endorser_client
          .new_ledger(tonic::Request::new(NewLedgerReq {
            handle: handle.to_bytes(),
          }))
          .await;
        (pk_bytes, response)
      });
      jobs.push(job);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let NewLedgerResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("create_ledger failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToCreateLedger);
        }
      } else {
        eprintln!("create_ledger failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToCreateLedger);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }

  pub async fn append_ledger(
    &self,
    ledger_handle: &Handle,
    block_hash: &NimbleDigest,
    tail_hash: &NimbleDigest,
  ) -> Result<Receipt, CoordinatorError> {
    let mut jobs = Vec::new();
    for (pk, ec) in self
      .store
      .read()
      .expect("Failed to get the read lock")
      .iter()
    {
      let mut endorser_client = ec.clone();
      let handle = *ledger_handle;
      let block = *block_hash;
      let tail = *tail_hash;
      let pk_bytes = pk.clone();
      let job = tokio::spawn(async move {
        let response = endorser_client
          .append(tonic::Request::new(AppendReq {
            handle: handle.to_bytes(),
            block_hash: block.to_bytes(),
            cond_updated_tail_hash: tail.to_bytes(),
          }))
          .await;
        (pk_bytes, response)
      });
      jobs.push(job);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let AppendResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("append_ledger failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToAppendLedger);
        }
      } else {
        eprintln!("append_ledger failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToAppendLedger);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }

  pub async fn read_ledger_tail(
    &self,
    ledger_handle: &Handle,
    client_nonce: &Nonce,
  ) -> Result<Receipt, CoordinatorError> {
    let mut jobs = Vec::new();
    for (pk, ec) in self
      .store
      .read()
      .expect("Failed to get the read lock")
      .iter()
    {
      let mut endorser_client = ec.clone();
      let handle = *ledger_handle;
      let nonce = *client_nonce;
      let pk_bytes = pk.clone();
      let job = tokio::spawn(async move {
        let response = endorser_client
          .read_latest(tonic::Request::new(ReadLatestReq {
            handle: handle.to_bytes(),
            nonce: nonce.get(),
          }))
          .await;
        (pk_bytes, response)
      });
      jobs.push(job);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let ReadLatestResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("read_ledger_tail failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToReadLedger);
        }
      } else {
        eprintln!("read_ledger_tail failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToReadLedger);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }

  pub async fn append_view_ledger(
    &self,
    endorsers: &[Vec<u8>],
    block_hash: &NimbleDigest,
    tail_hash: &NimbleDigest,
  ) -> Result<Receipt, CoordinatorError> {
    let mut jobs = Vec::new();
    if let Ok(conn_map) = self.store.read() {
      for pk in endorsers {
        if !conn_map.contains_key(pk) {
          eprintln!("No endorser has this public key {:?}", pk);
          return Err(CoordinatorError::InvalidEndorserPublicKey);
        }
        let mut endorser_client = conn_map[pk].clone();
        let block = *block_hash;
        let tail = *tail_hash;
        let pk_bytes = pk.clone();
        let job = tokio::spawn(async move {
          let response = endorser_client
            .append_view_ledger(tonic::Request::new(AppendViewLedgerReq {
              block_hash: block.to_bytes(),
              cond_updated_tail_hash: tail.to_bytes(),
            }))
            .await;
          (pk_bytes, response)
        });
        jobs.push(job);
      }
    } else {
      eprintln!("Failed to acquire the read lock");
      return Err(CoordinatorError::FailedToAcquireReadLock);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let AppendViewLedgerResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("append_view_ledger failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToAppendViewLedger);
        }
      } else {
        eprintln!("append_view_ledger failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToAppendViewLedger);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }

  #[allow(dead_code)]
  pub async fn read_view_ledger_tail(
    &self,
    client_nonce: &Nonce,
  ) -> Result<Receipt, CoordinatorError> {
    let mut jobs = Vec::new();
    for (pk, ec) in self
      .store
      .read()
      .expect("Failed to get the read lock")
      .iter()
    {
      let mut endorser_client = ec.clone();
      let nonce = *client_nonce;
      let pk_bytes = pk.clone();
      let job = tokio::spawn(async move {
        let response = endorser_client
          .read_latest_view_ledger(tonic::Request::new(ReadLatestViewLedgerReq {
            nonce: nonce.get(),
          }))
          .await;
        (pk_bytes, response)
      });
      jobs.push(job);
    }

    let mut receipt_bytes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for job in jobs {
      let res = job.await;
      if let Ok((pk, res2)) = res {
        if let Ok(resp) = res2 {
          let ReadLatestViewLedgerResp { signature } = resp.into_inner();
          receipt_bytes.push((pk, signature));
        } else {
          eprintln!("read_view_ledger_tail failed: {:?}", res2.unwrap_err());
          return Err(CoordinatorError::FailedToReadViewLedger);
        }
      } else {
        eprintln!("read_view_ledger_tail failed: {:?}", res.unwrap_err());
        return Err(CoordinatorError::FailedToReadViewLedger);
      }
    }
    let receipt = ledger::Receipt::from_bytes(&receipt_bytes);
    Ok(receipt)
  }
}
