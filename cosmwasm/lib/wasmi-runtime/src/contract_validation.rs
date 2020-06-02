use base64;
use enclave_ffi_types::{Ctx, EnclaveError};
use log::*;
use parity_wasm::elements;

use serde_json::Value;
use wasmi::{ImportsBuilder, ModuleInstance};

use super::results::{HandleSuccess, InitSuccess, QuerySuccess};

use crate::cosmwasm::encoding::Binary;
use crate::cosmwasm::types::{ContractResult, Env, Response};

use crate::crypto::{sha_256, AESKey, Hmac, Kdf, SIVEncryptable, HASH_SIZE, KEY_MANAGER};
use crate::errors::wasmi_error_to_enclave_error;
use crate::gas::{gas_rules, WasmCosts};
use crate::runtime::{Engine, EnigmaImportResolver, Runtime};

pub const CONTRACT_KEY_LENGTH: usize = HASH_SIZE + HASH_SIZE;

pub fn extract_contract_key(env: &Env) -> Result<[u8; CONTRACT_KEY_LENGTH], EnclaveError> {
    if env.contract_key.is_none() {
        error!("Contract execute with empty contract key");
        return Err(EnclaveError::FailedContractAuthentication);
    }

    let contract_key =
        base64::decode(env.contract_key.as_ref().unwrap().as_bytes()).map_err(|err| {
            error!(
                "got an error while trying to deserialize output bytes into json {:?}: {}",
                env, err
            );
            EnclaveError::FailedContractAuthentication
        })?;

    if contract_key.len() != CONTRACT_KEY_LENGTH {
        error!("Contract execute with empty contract key");
        return Err(EnclaveError::FailedContractAuthentication);
    }

    let mut key_as_bytes = [0u8; CONTRACT_KEY_LENGTH];

    key_as_bytes.copy_from_slice(&contract_key);

    Ok(key_as_bytes)
}

pub fn generate_sender_id(msg_sender: &[u8], block_height: u64) -> [u8; HASH_SIZE] {
    let mut input_data = msg_sender.to_vec();
    input_data.extend_from_slice(&block_height.to_be_bytes());
    sha_256(&input_data)
}

pub fn generate_contract_id(
    consensus_state_ikm: &AESKey,
    sender_id: &[u8; HASH_SIZE],
    code_hash: &[u8; HASH_SIZE],
) -> [u8; HASH_SIZE] {
    let authentication_key = consensus_state_ikm.derive_key_from_this(sender_id.as_ref());

    let mut input_data = sender_id.to_vec();
    input_data.extend_from_slice(code_hash);

    authentication_key.sign_sha_256(&input_data)
}

pub fn calc_contract_hash(contract_bytes: &[u8]) -> [u8; HASH_SIZE] {
    sha_256(&contract_bytes)
}

pub fn append_contract_key(
    response: &[u8],
    contract_key: [u8; 64],
) -> Result<Vec<u8>, EnclaveError> {
    debug!(
        "Append contract key -before: {:?}",
        String::from_utf8_lossy(&response)
    );

    let mut v: Value = serde_json::from_slice(response).map_err(|err| {
        error!(
            "got an error while trying to deserialize response bytes into json {:?}: {}",
            response, err
        );
        EnclaveError::FailedSeal
    })?;

    if let Value::Object(_) = &mut v["ok"] {
        v["ok"]["contract_key"] = Value::String(base64::encode(contract_key.to_vec().as_slice()));
    }

    let output = serde_json::ser::to_vec(&v).map_err(|err| {
        error!(
            "got an error while trying to serialize output json into bytes {:?}: {}",
            v, err
        );
        EnclaveError::FailedSeal
    })?;

    debug!(
        "Append contract key - after: {:?}",
        String::from_utf8_lossy(&output)
    );

    Ok(output)
}

pub fn validate_contract_key(
    contract_key: &[u8; CONTRACT_KEY_LENGTH],
    contract_code: &[u8],
) -> bool {
    // parse contract key -> < signer_id || authentication_code >
    let mut signer_id: [u8; HASH_SIZE] = [0u8; HASH_SIZE];
    signer_id.copy_from_slice(&contract_key[0..HASH_SIZE]);

    let mut expected_authentication_id: [u8; HASH_SIZE] = [0u8; HASH_SIZE];
    expected_authentication_id.copy_from_slice(&contract_key[HASH_SIZE..]);

    // calculate contract hash
    let contract_hash = calc_contract_hash(contract_code);

    // get the enclave key
    let enclave_key = KEY_MANAGER
        .get_consensus_state_ikm()
        .map_err(|err| {
            error!("Error extractling consensus_state_key");
            return false;
        })
        .unwrap();

    // calculate the authentication_id
    let calculated_authentication_id =
        generate_contract_id(&enclave_key, &signer_id, &contract_hash);

    return calculated_authentication_id == expected_authentication_id;
}
