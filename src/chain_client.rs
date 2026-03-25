use crate::types::*;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use subxt::backend::legacy::LegacyRpcMethods;
use subxt::config::substrate::H256;
use subxt::ext::subxt_rpcs::RpcClient;
use subxt::{dynamic::Value, OnlineClient, PolkadotConfig};
use tokio::sync::Semaphore;

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 500;

type ScaleValue = subxt::ext::scale_value::Value<u32>;
type ScaleComposite = subxt::ext::scale_value::Composite<u32>;
use subxt::ext::scale_value::ValueDef;

pub struct ChainClient {
	api: OnlineClient<PolkadotConfig>,
	rpc: LegacyRpcMethods<PolkadotConfig>,
	rpc_client: RpcClient,
	para_id: u32,
	semaphore: Arc<Semaphore>,
}

impl ChainClient {
	pub async fn new(rpc_url: &str, para_id: u32, concurrency: usize) -> Result<Self> {
		tracing::info!(rpc_url, "Connecting to relay chain...");
		let rpc_client = RpcClient::from_insecure_url(rpc_url)
			.await
			.context("Failed to connect to relay chain RPC")?;
		let api = OnlineClient::<PolkadotConfig>::from_rpc_client(rpc_client.clone()).await?;
		let rpc = LegacyRpcMethods::<PolkadotConfig>::new(rpc_client.clone());
		tracing::info!("Connected to relay chain");
		Ok(Self { api, rpc, rpc_client, para_id, semaphore: Arc::new(Semaphore::new(concurrency)) })
	}

	pub async fn block_hash_at(&self, block_number: u32) -> Result<H256> {
		let _permit = self.semaphore.acquire().await?;
		let hash = self
			.rpc
			.chain_get_block_hash(Some(block_number.into()))
			.await?
			.context(format!("No block hash for block #{}", block_number))?;
		Ok(hash)
	}

	pub async fn session_index_at(&self, block_hash: H256) -> Result<u32> {
		let _permit = self.semaphore.acquire().await?;
		let query = subxt::dynamic::storage("Session", "CurrentIndex", ());
		let result = self
			.api
			.storage()
			.at(block_hash)
			.fetch(&query)
			.await?
			.context("Session::CurrentIndex not found")?;
		let val = result.to_value()?;
		extract_u32(&val).context("Failed to decode session index")
	}

	/// Find session boundaries in the relay block range using binary search.
	pub async fn find_session_boundaries(
		&self,
		start_block: u32,
		end_block: u32,
	) -> Result<Vec<(u32, u32, u32)>> {
		if start_block >= end_block {
			return Ok(vec![]);
		}

		let start_hash = self.block_hash_at(start_block).await?;
		let end_hash = self.block_hash_at(end_block).await?;
		let start_session = self.session_index_at(start_hash).await?;
		let end_session = self.session_index_at(end_hash).await?;

		if start_session == end_session {
			tracing::info!(
				session = start_session,
				"No session changes in relay block range {}-{}",
				start_block,
				end_block
			);
			return Ok(vec![]);
		}

		tracing::info!(
			start_session,
			end_session,
			"Found {} session changes, binary searching for boundaries",
			end_session - start_session
		);

		let mut boundaries = Vec::new();
		for target_session in (start_session + 1)..=end_session {
			let boundary =
				self.binary_search_session_start(start_block, end_block, target_session).await?;
			let prev_session = target_session - 1;
			boundaries.push((boundary, prev_session, target_session));
			tracing::info!(
				boundary_block = boundary,
				session_before = prev_session,
				session_after = target_session,
				"Session boundary found"
			);
		}
		Ok(boundaries)
	}

	async fn binary_search_session_start(
		&self,
		mut lo: u32,
		mut hi: u32,
		target_session: u32,
	) -> Result<u32> {
		while lo < hi {
			let mid = lo + (hi - lo) / 2;
			let mid_hash = self.block_hash_at(mid).await?;
			let session = self.session_index_at(mid_hash).await?;
			if session < target_session {
				lo = mid + 1;
			} else {
				hi = mid;
			}
		}
		Ok(lo)
	}

	/// Query a range of relay blocks for para inclusion events.
	/// Returns a BTreeMap of block_number -> OnChainBlockInfo.
	pub async fn query_relay_block_range(
		&self,
		blocks: &[u32],
	) -> Result<BTreeMap<u32, OnChainBlockInfo>> {
		let mut results = BTreeMap::new();
		let rpc_failures = Arc::new(AtomicU32::new(0));

		let mut handles = Vec::new();
		for &block_num in blocks {
			let api = self.api.clone();
			let rpc = LegacyRpcMethods::<PolkadotConfig>::new(self.rpc_client.clone());
			let para_id = self.para_id;
			let sem = self.semaphore.clone();
			let failures = rpc_failures.clone();

			handles.push(tokio::spawn(async move {
				// Fetch block hash with retry
				let hash = retry_async(|| async {
					let _permit = sem.acquire().await.unwrap();
					match rpc.chain_get_block_hash(Some(block_num.into())).await {
						Ok(Some(h)) => Ok(h),
						Ok(None) => Err(anyhow::anyhow!("No hash for block #{}", block_num)),
						Err(e) => Err(e.into()),
					}
				})
				.await;

				let hash = match hash {
					Ok(h) => h,
					Err(_) => {
						failures.fetch_add(1, Ordering::Relaxed);
						return (block_num, None);
					},
				};

				// Fetch session index with retry
				let session_index = retry_async(|| async {
					let _permit = sem.acquire().await.unwrap();
					let query = subxt::dynamic::storage("Session", "CurrentIndex", ());
					let result = api
						.storage()
						.at(hash)
						.fetch(&query)
						.await
						.map_err(|e| anyhow::anyhow!(e))?
						.ok_or_else(|| anyhow::anyhow!("Session::CurrentIndex not found"))?;
					let val = result.to_value().map_err(|e| anyhow::anyhow!(e))?;
					extract_u32(&val)
						.ok_or_else(|| anyhow::anyhow!("Failed to decode session index"))
				})
				.await
				.unwrap_or_else(|_| {
					failures.fetch_add(1, Ordering::Relaxed);
					0
				});

				// Fetch events with retry
				let events_result = retry_async(|| async {
					let _permit = sem.acquire().await.unwrap();
					api.events()
						.at(hash)
						.await
						.map_err(|e| anyhow::anyhow!(e))
				})
				.await;

				let mut backed = Vec::new();
				let mut included = Vec::new();

				match events_result {
					Ok(events) => {
						for event in events.iter() {
							let Ok(event) = event else { continue };
							if event.pallet_name() != "ParaInclusion" {
								continue;
							}
							let is_backed = event.variant_name() == "CandidateBacked";
							let is_included = event.variant_name() == "CandidateIncluded";
							if !is_backed && !is_included {
								continue;
							}
							if let Ok(fields) = event.field_values() {
								if let Some(ce) =
									extract_para_candidate_event(&fields, para_id)
								{
									if is_backed {
										backed.push(ce);
									} else {
										included.push(ce);
									}
								}
							}
						}
					},
					Err(_) => {
						failures.fetch_add(1, Ordering::Relaxed);
					},
				}

				let info = OnChainBlockInfo {
					block_number: block_num,
					block_hash: hash.0,
					session_index,
					backed_para_candidates: backed,
					included_para_candidates: included,
				};

				(block_num, Some(info))
			}));
		}

		for handle in handles {
			let (block_num, info) = handle.await?;
			if let Some(info) = info {
				results.insert(block_num, info);
			}
		}

		let total_failures = rpc_failures.load(Ordering::Relaxed);
		if total_failures > 0 {
			tracing::warn!(
				failures = total_failures,
				total_queries = blocks.len(),
				"Some RPC queries failed after retries — results may be incomplete"
			);
		}

		Ok(results)
	}

	/// Query Paras::Heads to get the current para head block number at a given relay block.
	#[allow(dead_code)]
	pub async fn para_head_at(&self, block_hash: H256) -> Result<Option<u32>> {
		let _permit = self.semaphore.acquire().await?;
		let query =
			subxt::dynamic::storage("Paras", "Heads", vec![Value::u128(self.para_id as u128)]);
		let result = self.api.storage().at(block_hash).fetch(&query).await?;

		if let Some(val) = result {
			let decoded = val.to_value()?;
			if let Some(bytes) = extract_bytes(&decoded) {
				return Ok(decode_para_block_number(&bytes));
			}
		}
		Ok(None)
	}
}

/// Extract a u32 from a dynamic Value.
fn extract_u32(val: &ScaleValue) -> Option<u32> {
	val.as_u128().map(|v| v as u32)
}

/// Recursively extract a u32 from a Value, handling newtype wrappers.
/// E.g., ParaId(u32) is encoded as Composite::Unnamed([Value::Primitive(u128)])
fn extract_u32_recursive(val: &ScaleValue) -> Option<u32> {
	// Try direct primitive first
	if let Some(n) = val.as_u128() {
		return Some(n as u32);
	}
	// Try unwrapping one level of composite (newtype wrapper)
	if let Some(inner) = get_first_value(val) {
		if let Some(n) = inner.as_u128() {
			return Some(n as u32);
		}
		// Try two levels deep
		if let Some(inner2) = get_first_value(inner) {
			if let Some(n) = inner2.as_u128() {
				return Some(n as u32);
			}
		}
	}
	None
}

/// Extract bytes from a dynamic Value.
/// Handles both flat `Composite([u8, u8, ...])` and newtype wrappers like
/// `HeadData(Vec<u8>)` which decode as `Composite([Composite([u8, u8, ...])])`.
fn extract_bytes(val: &ScaleValue) -> Option<Vec<u8>> {
	fn try_composite_bytes(composite: &ScaleComposite) -> Option<Vec<u8>> {
		let bytes: Vec<u8> = composite
			.values()
			.filter_map(|v| v.as_u128().map(|b| b as u8))
			.collect();
		if !bytes.is_empty() {
			return Some(bytes);
		}
		// Try unwrapping one level of newtype wrapper:
		// Composite([Composite([u8, u8, ...])])
		let first = composite.values().next()?;
		match &first.value {
			ValueDef::Composite(inner) => {
				let bytes: Vec<u8> = inner
					.values()
					.filter_map(|v| v.as_u128().map(|b| b as u8))
					.collect();
				if !bytes.is_empty() {
					Some(bytes)
				} else {
					None
				}
			},
			_ => None,
		}
	}

	match &val.value {
		ValueDef::Composite(composite) => try_composite_bytes(composite),
		_ => None,
	}
}

/// Decode para block number from HeadData bytes.
/// HeadData is the SCALE-encoded para header:
///   parent_hash: [u8; 32] + Compact<u32> number + ...
pub fn decode_para_block_number(head_data: &[u8]) -> Option<u32> {
	use codec::Decode;
	if head_data.len() < 33 {
		return None;
	}
	// Skip 32 bytes of parent_hash
	let mut cursor = &head_data[32..];
	let number = codec::Compact::<u32>::decode(&mut cursor).ok()?;
	Some(number.0)
}

/// Compute the block hash from HeadData bytes (blake2b-256 of the header).
pub fn compute_block_hash_from_head_data(head_data: &[u8]) -> [u8; 32] {
	use blake2::digest::{consts::U32, Digest};
	type Blake2b256 = blake2::Blake2b<U32>;
	let mut hasher = Blake2b256::new();
	hasher.update(head_data);
	let result = hasher.finalize();
	let mut hash = [0u8; 32];
	hash.copy_from_slice(&result);
	hash
}

/// Extract ParaCandidateEvent from a CandidateBacked/CandidateIncluded event's field values.
/// Event structure: (CandidateReceipt, HeadData, CoreIndex, GroupIndex)
/// CandidateReceipt: { descriptor: { para_id, relay_parent, ... }, commitments_hash }
fn extract_para_candidate_event(
	fields: &ScaleComposite,
	our_para_id: u32,
) -> Option<ParaCandidateEvent> {
	// Collect values from composite (either Named or Unnamed)
	let field_values: Vec<&ScaleValue> = fields.values().collect();
	if field_values.is_empty() {
		tracing::trace!("Event has no field values");
		return None;
	}

	// First field: CandidateReceipt
	let receipt = field_values[0];

	// Navigate: receipt -> descriptor -> para_id
	let descriptor = get_named_field(receipt, "descriptor")?;
	let para_id_val = get_named_field(descriptor, "para_id")?;
	// ParaId may be a direct u128, or a newtype Composite wrapping u32
	let para_id = extract_u32_recursive(para_id_val)?;

	if para_id != our_para_id {
		return None;
	}

	let relay_parent = get_named_field(descriptor, "relay_parent").and_then(extract_h256);

	// Third field: CoreIndex (a newtype wrapping u32)
	let core_index = field_values.get(2).and_then(|v| {
		// CoreIndex might be a composite wrapping u32 or a direct u32
		v.as_u128()
			.map(|n| n as u32)
			.or_else(|| get_first_value(v).and_then(|v| v.as_u128()).map(|n| n as u32))
	});

	// Fourth field: GroupIndex
	let group_index = field_values.get(3).and_then(|v| {
		v.as_u128()
			.map(|n| n as u32)
			.or_else(|| get_first_value(v).and_then(|v| v.as_u128()).map(|n| n as u32))
	});

	// Second field: HeadData
	let head_data = field_values.get(1).and_then(|v| extract_bytes(v));

	Some(ParaCandidateEvent {
		para_id,
		relay_parent,
		core_index,
		group_index,
		para_head_data: head_data,
	})
}

/// Get a named field from a Value that wraps a Named composite.
fn get_named_field<'a>(val: &'a ScaleValue, name: &str) -> Option<&'a ScaleValue> {
	match &val.value {
		ValueDef::Composite(subxt::ext::scale_value::Composite::Named(fields)) => {
			for (field_name, field_val) in fields {
				if field_name == name {
					return Some(field_val);
				}
			}
			None
		},
		_ => None,
	}
}

/// Get the first value from a composite (for newtype wrappers like CoreIndex(u32)).
fn get_first_value(val: &ScaleValue) -> Option<&ScaleValue> {
	match &val.value {
		ValueDef::Composite(composite) => composite.values().next(),
		_ => None,
	}
}

/// Extract H256 (32 bytes) from a Value — typically a composite of 32 u8 values.
fn extract_h256(val: &ScaleValue) -> Option<[u8; 32]> {
	match &val.value {
		ValueDef::Composite(composite) => {
			let bytes: Vec<u8> = composite
				.values()
				.filter_map(|v| v.as_u128().map(|b| b as u8))
				.collect();
			if bytes.len() == 32 {
				let mut arr = [0u8; 32];
				arr.copy_from_slice(&bytes);
				Some(arr)
			} else {
				None
			}
		},
		_ => None,
	}
}

/// Retry an async operation with exponential backoff.
async fn retry_async<F, Fut, T>(mut f: F) -> Result<T>
where
	F: FnMut() -> Fut,
	Fut: std::future::Future<Output = Result<T>>,
{
	let mut last_err = None;
	for attempt in 0..=MAX_RETRIES {
		match f().await {
			Ok(val) => return Ok(val),
			Err(e) => {
				last_err = Some(e);
				if attempt < MAX_RETRIES {
					let delay = RETRY_BASE_MS * (1 << attempt);
					tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
				}
			},
		}
	}
	Err(last_err.unwrap())
}
