use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zksync_system_constants::{
    BOOTLOADER_ADDRESS, KNOWN_CODES_STORAGE_ADDRESS, L1_MESSENGER_ADDRESS,
    PUBLISH_BYTECODE_OVERHEAD,
};
use zksync_types::{
    bytecode::BytecodeHash,
    ethabi,
    l2_to_l1_log::{SystemL2ToL1Log, UserL2ToL1Log},
    zk_evm_types::FarCallOpcode,
    Address, L1BatchNumber, StorageLogWithPreviousValue, Transaction, H256, U256,
};

use crate::{
    BytecodeCompressionError, Halt, VmExecutionMetrics, VmExecutionStatistics, VmRevertReason,
};

/// Event generated by the VM.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct VmEvent {
    pub location: (L1BatchNumber, u32),
    pub address: Address,
    pub indexed_topics: Vec<H256>,
    pub value: Vec<u8>,
}

impl VmEvent {
    /// Long signature of the contract deployment event (`ContractDeployed`).
    pub const DEPLOY_EVENT_SIGNATURE: H256 = H256([
        41, 10, 253, 174, 35, 26, 63, 192, 187, 174, 139, 26, 246, 54, 152, 176, 161, 215, 155, 33,
        173, 23, 223, 3, 66, 223, 185, 82, 254, 116, 248, 229,
    ]);
    /// Long signature of the L1 messenger bytecode publication event (`BytecodeL1PublicationRequested`).
    pub const L1_MESSENGER_BYTECODE_PUBLICATION_EVENT_SIGNATURE: H256 = H256([
        72, 13, 60, 159, 114, 123, 94, 92, 18, 3, 212, 198, 31, 177, 133, 211, 127, 8, 230, 178,
        220, 94, 155, 191, 152, 89, 27, 26, 122, 221, 245, 124,
    ]);
    /// Long signature of the known bytecodes storage bytecode publication event (`MarkedAsKnown`).
    pub const PUBLISHED_BYTECODE_SIGNATURE: H256 = H256([
        201, 71, 34, 255, 19, 234, 207, 83, 84, 124, 71, 65, 218, 181, 34, 131, 83, 160, 89, 56,
        255, 205, 213, 212, 162, 213, 51, 174, 14, 97, 130, 135,
    ]);
    /// Long signature of the L1 messenger publication event (`L1MessageSent`).
    pub const L1_MESSAGE_EVENT_SIGNATURE: H256 = H256([
        58, 54, 228, 114, 145, 244, 32, 31, 175, 19, 127, 171, 8, 29, 146, 41, 91, 206, 45, 83,
        190, 44, 108, 166, 139, 168, 44, 127, 170, 156, 226, 65,
    ]);

    /// Extracts all the "long" L2->L1 messages that were submitted by the L1Messenger contract.
    pub fn extract_long_l2_to_l1_messages(events: &[Self]) -> Vec<Vec<u8>> {
        events
            .iter()
            .filter(|event| {
                // Filter events from the l1 messenger contract that match the expected signature.
                event.address == L1_MESSENGER_ADDRESS
                    && event.indexed_topics.len() == 3
                    && event.indexed_topics[0] == Self::L1_MESSAGE_EVENT_SIGNATURE
            })
            .map(|event| {
                let decoded_tokens = ethabi::decode(&[ethabi::ParamType::Bytes], &event.value)
                    .expect("Failed to decode L1MessageSent message");
                // The `Token` does not implement `Copy` trait, so I had to do it like that:
                let bytes_token = decoded_tokens.into_iter().next().unwrap();
                bytes_token.into_bytes().unwrap()
            })
            .collect()
    }

    /// Extracts bytecodes that were marked as known on the system contracts and should be published onchain.
    pub fn extract_published_bytecodes(events: &[Self]) -> Vec<H256> {
        events
            .iter()
            .filter(|event| {
                // Filter events from the deployer contract that match the expected signature.
                event.address == KNOWN_CODES_STORAGE_ADDRESS
                    && event.indexed_topics.len() == 3
                    && event.indexed_topics[0] == Self::PUBLISHED_BYTECODE_SIGNATURE
                    && event.indexed_topics[2] != H256::zero()
            })
            .map(|event| event.indexed_topics[1])
            .collect()
    }

    /// Extracts all bytecodes marked as known on the system contracts.
    pub fn extract_bytecodes_marked_as_known(events: &[Self]) -> impl Iterator<Item = H256> + '_ {
        events
            .iter()
            .filter(|event| {
                // Filter events from the deployer contract that match the expected signature.
                event.address == KNOWN_CODES_STORAGE_ADDRESS
                    && event.indexed_topics.len() == 3
                    && event.indexed_topics[0] == Self::PUBLISHED_BYTECODE_SIGNATURE
            })
            .map(|event| event.indexed_topics[1])
    }

    /// Returns `true` if any of the given events is a `ContractDeployed` event.
    pub fn contains_contract_deployment(events: &[Self]) -> bool {
        events.iter().any(|event| {
            // We expect that the first indexed topic is the event signature.
            event
                .indexed_topics
                .get(0)
                .map_or(false, |&topic| topic == VmEvent::DEPLOY_EVENT_SIGNATURE)
        })
    }
}

/// Refunds produced for the user.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Refunds {
    pub gas_refunded: u64,
    pub operator_suggested_refund: u64,
}

/// Events/storage logs/l2->l1 logs created within transaction execution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VmExecutionLogs {
    pub storage_logs: Vec<StorageLogWithPreviousValue>,
    pub events: Vec<VmEvent>,
    // For pre-boojum VMs, there was no distinction between user logs and system
    // logs and so all the outputted logs were treated as user_l2_to_l1_logs.
    pub user_l2_to_l1_logs: Vec<UserL2ToL1Log>,
    pub system_l2_to_l1_logs: Vec<SystemL2ToL1Log>,
    // This field moved to statistics, but we need to keep it for backward compatibility
    pub total_log_queries_count: usize,
}

impl VmExecutionLogs {
    pub fn total_l2_to_l1_logs_count(&self) -> usize {
        self.user_l2_to_l1_logs.len() + self.system_l2_to_l1_logs.len()
    }
}

/// Result and logs of the VM execution.
#[derive(Debug, Clone)]
pub struct VmExecutionResultAndLogs {
    pub result: ExecutionResult,
    pub logs: VmExecutionLogs,
    pub statistics: VmExecutionStatistics,
    pub refunds: Refunds,
    /// Dynamic bytecodes decommitted during VM execution (i.e., not present in the storage at the start of VM execution
    /// or in `factory_deps` fields of executed transactions). Currently, the only kind of such codes are EVM bytecodes.
    /// Correspondingly, they may only be present if supported by the VM version, and if the VM is initialized with the EVM emulator base system contract.
    pub dynamic_factory_deps: HashMap<H256, Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionResult {
    /// Returned successfully
    Success { output: Vec<u8> },
    /// Reverted by contract
    Revert { output: VmRevertReason },
    /// Reverted for various reasons
    Halt { reason: Halt },
}

impl ExecutionResult {
    /// Returns `true` if the execution was failed.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Revert { .. } | Self::Halt { .. })
    }
}

impl VmExecutionResultAndLogs {
    /// Creates a mock full result based on the provided base result.
    pub fn mock(result: ExecutionResult) -> Self {
        Self {
            result,
            logs: VmExecutionLogs::default(),
            statistics: VmExecutionStatistics::default(),
            refunds: Refunds::default(),
            dynamic_factory_deps: HashMap::new(),
        }
    }

    /// Creates a mock successful result with no payload.
    pub fn mock_success() -> Self {
        Self::mock(ExecutionResult::Success { output: vec![] })
    }

    pub fn get_execution_metrics(&self) -> VmExecutionMetrics {
        // We published the data as ABI-encoded `bytes`, so the total length is:
        // - message length in bytes, rounded up to a multiple of 32
        // - 32 bytes of encoded offset
        // - 32 bytes of encoded length
        let l2_l1_long_messages = VmEvent::extract_long_l2_to_l1_messages(&self.logs.events)
            .iter()
            .map(|event| (event.len() + 31) / 32 * 32 + 64)
            .sum();

        let published_bytecode_bytes = VmEvent::extract_published_bytecodes(&self.logs.events)
            .iter()
            .map(|&bytecode_hash| {
                let len_in_bytes = BytecodeHash::try_from(bytecode_hash)
                    .expect("published unparseable bytecode hash")
                    .len_in_bytes();
                len_in_bytes + PUBLISH_BYTECODE_OVERHEAD as usize
            })
            .sum();

        // Count how many contracts were deployed
        let contract_deployment_count =
            VmEvent::extract_bytecodes_marked_as_known(&self.logs.events).count();

        VmExecutionMetrics {
            gas_used: self.statistics.gas_used as usize,
            published_bytecode_bytes,
            l2_l1_long_messages,
            l2_to_l1_logs: self.logs.total_l2_to_l1_logs_count(),
            user_l2_to_l1_logs: self.logs.user_l2_to_l1_logs.len(),
            contracts_used: self.statistics.contracts_used,
            vm_events: self.logs.events.len(),
            storage_logs: self.logs.storage_logs.len(),
            total_log_queries: self.statistics.total_log_queries,
            cycles_used: self.statistics.cycles_used,
            computational_gas_used: self.statistics.computational_gas_used,
            pubdata_published: self.statistics.pubdata_published,
            circuit_statistic: self.statistics.circuit_statistic,
            contract_deployment_count: contract_deployment_count,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TxExecutionStatus {
    Success,
    Failure,
}

impl TxExecutionStatus {
    pub fn from_has_failed(has_failed: bool) -> Self {
        if has_failed {
            Self::Failure
        } else {
            Self::Success
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum CallType {
    #[serde(serialize_with = "far_call_type_to_u8")]
    #[serde(deserialize_with = "far_call_type_from_u8")]
    Call(FarCallOpcode),
    Create,
    NearCall,
}

impl Default for CallType {
    fn default() -> Self {
        Self::Call(FarCallOpcode::Normal)
    }
}

fn far_call_type_from_u8<'de, D>(deserializer: D) -> Result<FarCallOpcode, D::Error>
where
    D: Deserializer<'de>,
{
    let res = u8::deserialize(deserializer)?;
    match res {
        0 => Ok(FarCallOpcode::Normal),
        1 => Ok(FarCallOpcode::Delegate),
        2 => Ok(FarCallOpcode::Mimic),
        _ => Err(serde::de::Error::custom("Invalid FarCallOpcode")),
    }
}

fn far_call_type_to_u8<S>(far_call_type: &FarCallOpcode, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_u8(*far_call_type as u8)
}

/// Represents a call in the VM trace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Call {
    /// Type of the call.
    pub r#type: CallType,
    /// Address of the caller.
    pub from: Address,
    /// Address of the callee.
    pub to: Address,
    /// Gas from the parent call.
    pub parent_gas: u64,
    /// Gas provided for the call.
    pub gas: u64,
    /// Gas used by the call.
    pub gas_used: u64,
    /// Value transferred.
    pub value: U256,
    /// Input data.
    pub input: Vec<u8>,
    /// Output data.
    pub output: Vec<u8>,
    /// Error message provided by vm or some unexpected errors.
    pub error: Option<String>,
    /// Revert reason.
    pub revert_reason: Option<String>,
    /// Subcalls.
    pub calls: Vec<Call>,
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        self.revert_reason == other.revert_reason
            && self.input == other.input
            && self.from == other.from
            && self.to == other.to
            && self.r#type == other.r#type
            && self.value == other.value
            && self.error == other.error
            && self.output == other.output
            && self.calls == other.calls
    }
}

impl Call {
    pub fn new_high_level(
        gas: u64,
        gas_used: u64,
        value: U256,
        input: Vec<u8>,
        output: Vec<u8>,
        revert_reason: Option<String>,
        calls: Vec<Call>,
    ) -> Self {
        Self {
            r#type: CallType::Call(FarCallOpcode::Normal),
            from: Address::zero(),
            to: BOOTLOADER_ADDRESS,
            parent_gas: gas,
            gas,
            gas_used,
            value,
            input,
            output,
            error: None,
            revert_reason,
            calls,
        }
    }
}

/// Mid-level transaction execution output returned by a [batch executor](crate::executor::BatchExecutor).
#[derive(Debug)]
pub struct BatchTransactionExecutionResult {
    /// VM result.
    pub tx_result: Box<VmExecutionResultAndLogs>,
    /// Compressed bytecodes used by the transaction.
    pub compression_result: Result<(), BytecodeCompressionError>,
    /// Call traces (if requested; otherwise, empty).
    pub call_traces: Vec<Call>,
}

impl BatchTransactionExecutionResult {
    pub fn was_halted(&self) -> bool {
        matches!(self.tx_result.result, ExecutionResult::Halt { .. })
    }
}

/// Mid-level transaction execution output returned by a [oneshot executor](crate::executor::OneshotExecutor).
pub type OneshotTransactionExecutionResult = BatchTransactionExecutionResult;

/// High-level transaction execution result used by the state keeper etc.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionExecutionResult {
    pub transaction: Transaction,
    pub hash: H256,
    pub execution_info: VmExecutionMetrics,
    pub execution_status: TxExecutionStatus,
    pub refunded_gas: u64,
    pub call_traces: Vec<Call>,
    pub revert_reason: Option<String>,
}

impl TransactionExecutionResult {
    pub fn call_trace(&self) -> Option<Call> {
        if self.call_traces.is_empty() {
            None
        } else {
            Some(Call::new_high_level(
                self.transaction.gas_limit().as_u64(),
                self.transaction.gas_limit().as_u64() - self.refunded_gas,
                self.transaction.execute.value,
                self.transaction.execute.calldata.clone(),
                vec![],
                self.revert_reason.clone(),
                self.call_traces.clone(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use zksync_types::ethabi;

    use super::*;

    #[test]
    fn deploy_event_signature_matches() {
        let expected_signature = ethabi::long_signature(
            "ContractDeployed",
            &[
                ethabi::ParamType::Address,
                ethabi::ParamType::FixedBytes(32),
                ethabi::ParamType::Address,
            ],
        );
        assert_eq!(VmEvent::DEPLOY_EVENT_SIGNATURE, expected_signature);
    }

    #[test]
    fn bytecode_publication_request_event_signature_matches() {
        let expected_signature = ethabi::long_signature(
            "BytecodeL1PublicationRequested",
            &[ethabi::ParamType::FixedBytes(32)],
        );
        assert_eq!(
            VmEvent::L1_MESSENGER_BYTECODE_PUBLICATION_EVENT_SIGNATURE,
            expected_signature
        );
    }

    #[test]
    fn l1_message_event_signature_matches() {
        let expected_signature = ethabi::long_signature(
            "L1MessageSent",
            &[
                ethabi::ParamType::Address,
                ethabi::ParamType::FixedBytes(32),
                ethabi::ParamType::Bytes,
            ],
        );
        assert_eq!(VmEvent::L1_MESSAGE_EVENT_SIGNATURE, expected_signature);
    }

    #[test]
    fn published_bytecode_event_signature_matches() {
        let expected_signature = ethabi::long_signature(
            "MarkedAsKnown",
            &[ethabi::ParamType::FixedBytes(32), ethabi::ParamType::Bool],
        );
        assert_eq!(VmEvent::PUBLISHED_BYTECODE_SIGNATURE, expected_signature);
    }
}
