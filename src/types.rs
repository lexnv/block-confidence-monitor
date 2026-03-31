use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// A hash that may be truncated (from log) or full.
/// Truncated format: "0xabcd…ef01" (first 2 bytes + last 2 bytes)
#[derive(Clone, Debug)]
pub struct LogHash {
	/// Raw string as it appeared in the log (e.g. "0xabcd…ef01" or full hex)
	pub raw: String,
	/// Full 32-byte hash if known
	pub full: Option<[u8; 32]>,
}

impl LogHash {
	pub fn from_truncated(s: &str) -> Self {
		Self { raw: s.to_string(), full: None }
	}

	pub fn from_full(s: &str) -> Self {
		let full = parse_hex_hash(s);
		Self { raw: s.to_string(), full }
	}

	pub fn from_auto(s: &str) -> Self {
		if s.contains('…') || s.len() < 66 {
			Self::from_truncated(s)
		} else {
			Self::from_full(s)
		}
	}

	/// Try to resolve this hash against a registry of known full hashes.
	pub fn resolve(&mut self, registry: &HashRegistry) {
		if self.full.is_some() {
			return;
		}
		if let Some(full) = registry.resolve(&self.raw) {
			self.full = Some(full);
		}
	}

	pub fn short(&self) -> &str {
		if self.raw.len() > 14 {
			&self.raw[..14]
		} else {
			&self.raw
		}
	}

	pub fn matches_full(&self, full: &[u8; 32]) -> bool {
		if let Some(ref our_full) = self.full {
			return our_full == full;
		}
		// Match truncated: "0xabcd…ef01" against full hash
		let hex = hex::encode(full);
		let full_str = format!("0x{hex}");
		let raw = &self.raw;
		if raw.contains('…') {
			let parts: Vec<&str> = raw.split('…').collect();
			if parts.len() == 2 {
				return full_str.starts_with(parts[0]) && full_str.ends_with(parts[1]);
			}
		}
		false
	}
}

impl fmt::Display for LogHash {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(ref full) = self.full {
			write!(f, "0x{}", hex::encode(full))
		} else {
			write!(f, "{}", self.raw)
		}
	}
}

pub fn parse_hex_hash(s: &str) -> Option<[u8; 32]> {
	let s = s.strip_prefix("0x").unwrap_or(s);
	if s.len() != 64 {
		return None;
	}
	let bytes = hex::decode(s).ok()?;
	let mut arr = [0u8; 32];
	arr.copy_from_slice(&bytes);
	Some(arr)
}

/// Registry mapping truncated hash patterns to full hashes.
#[derive(Default)]
pub struct HashRegistry {
	/// Map from (prefix_4chars, suffix_4chars) -> full hash
	pub by_truncated: HashMap<(String, String), [u8; 32]>,
	/// Map from full hash hex -> full hash bytes
	pub by_full: HashMap<String, [u8; 32]>,
}

impl HashRegistry {
	pub fn register_full(&mut self, full_hex: &str) {
		if let Some(hash) = parse_hex_hash(full_hex) {
			let normalized = format!("0x{}", hex::encode(&hash));
			self.by_full.insert(normalized.clone(), hash);
			// Also register truncated form
			if normalized.len() >= 10 {
				let prefix = normalized[..6].to_string(); // "0xabcd"
				let suffix = normalized[normalized.len() - 4..].to_string(); // "ef01"
				self.by_truncated.insert((prefix, suffix), hash);
			}
		}
	}

	pub fn resolve(&self, raw: &str) -> Option<[u8; 32]> {
		// Try full match first
		if let Some(hash) = self.by_full.get(raw) {
			return Some(*hash);
		}
		// Try truncated match
		if raw.contains('…') {
			let parts: Vec<&str> = raw.split('…').collect();
			if parts.len() == 2 {
				let key = (parts[0].to_string(), parts[1].to_string());
				if let Some(hash) = self.by_truncated.get(&key) {
					return Some(*hash);
				}
			}
		}
		None
	}
}

// ---- Parsed log event types ----

#[derive(Clone, Debug)]
pub struct ParaBlockImport {
	pub block_number: u32,
	pub parent_hash: LogHash,
	pub block_hash: LogHash,
	pub timestamp: DateTime<Utc>,
	/// true = 🏆 (best/new block), false = 🆕 (non-best/reorg)
	pub is_best: bool,
	/// Which collator this event came from (set in multi-collator mode)
	pub collator: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RelayBlockImport {
	pub block_number: u32,
	pub parent_hash: LogHash,
	pub block_hash: LogHash,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct BuildAttempt {
	pub building: bool, // true = "Building block", false = "Not building block"
	pub unincluded_segment_len: u32,
	pub relay_parent: LogHash,
	pub relay_parent_num: u32,
	pub relay_parent_offset: Option<u32>,
	pub included_hash: LogHash,
	pub included_num: u32,
	pub parent: LogHash,
	pub slot: u64,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct PreSealedBlock {
	pub block_number: u32,
	pub post_hash: LogHash, // full
	pub pre_hash: LogHash,  // full
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct CollationSubmission {
	pub core_index: u32,
	pub hash: LogHash, // full
	pub block_number: u32,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct PeerConnectionEvent {
	pub peer_id: String,
	pub connected: bool,
	/// Only present for PeerConnected events
	pub role: Option<String>,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct CollationFetchLatency {
	pub latency_ms: u64,
	pub para_head: LogHash, // full
	pub para_id: u32,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct CandidateGenerated {
	pub candidate_hash: LogHash,
	pub relay_parent: LogHash,
	pub para_id: u32,
	pub core_index: u32,
	pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct ViewUpdate {
	pub heads: Vec<String>,
	pub finalized_number: u32,
	pub timestamp: DateTime<Utc>,
}

/// A "Collation expired" log event from the collator protocol.
/// Shows why a collation was not included: the `collation_state` tells us
/// how far the collation got in the pipeline before it expired.
#[derive(Clone, Debug)]
pub struct CollationExpired {
	/// How far through the pipeline: "created", "advertised", "requested", "fetched", "backed"
	pub collation_state: String,
	/// Relay parent block number
	pub relay_parent_num: u32,
	/// Relay parent hash (may be truncated)
	pub relay_parent_hash: LogHash,
	/// Age in relay blocks when it expired
	pub age: Option<u32>,
	/// Para block head hash
	pub head: Option<LogHash>,
	/// Candidate hash
	pub candidate_hash: Option<LogHash>,
	/// Timestamp of the "Collation expired" log entry
	pub timestamp: DateTime<Utc>,
	// --- Enriched timeline timestamps (populated during analysis) ---
	/// When the block was sealed (from Pre-sealed block log)
	pub produced_at: Option<DateTime<Utc>>,
	/// When the collation was submitted/advertised (from Submitting collation log)
	pub advertised_at: Option<DateTime<Utc>>,
	/// When a validator fetched the collation (from Collation fetch latency log)
	pub fetched_at: Option<DateTime<Utc>>,
	/// Validator peer IDs this candidate was advertised to (from "Advertising collation" logs)
	pub advertised_to_peers: Vec<String>,
}

/// All events parsed from the log file, in chronological order.
#[derive(Default)]
pub struct ParsedLog {
	pub para_imports: Vec<ParaBlockImport>,
	pub relay_imports: Vec<RelayBlockImport>,
	pub build_attempts: Vec<BuildAttempt>,
	pub pre_sealed: Vec<PreSealedBlock>,
	pub collation_submissions: Vec<CollationSubmission>,
	pub collation_fetches: Vec<CollationFetchLatency>,
	pub candidates_generated: Vec<CandidateGenerated>,
	pub view_updates: Vec<ViewUpdate>,
	pub collation_expired: Vec<CollationExpired>,
	/// candidate_hash (raw string) → set of validator peer_ids advertised to
	pub advertising_peers: std::collections::HashMap<String, std::collections::BTreeSet<String>>,
	/// PeerConnected / PeerDisconnected events on the Collation peer set
	pub peer_connections: Vec<PeerConnectionEvent>,
	pub hash_registry: HashRegistry,
}

impl ParsedLog {
	pub fn min_relay_block(&self) -> Option<u32> {
		self.relay_imports.iter().map(|r| r.block_number).min()
	}

	pub fn max_relay_block(&self) -> Option<u32> {
		self.relay_imports.iter().map(|r| r.block_number).max()
	}

	pub fn time_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
		let all_times = self
			.para_imports
			.iter()
			.map(|e| e.timestamp)
			.chain(self.relay_imports.iter().map(|e| e.timestamp));
		let min = all_times.clone().min()?;
		let max = all_times.max()?;
		Some((min, max))
	}

	/// 🏆 best block imports (blocks built)
	pub fn best_imports(&self) -> usize {
		self.para_imports.iter().filter(|i| i.is_best).count()
	}

	/// 🆕 non-best block imports (blocks rebuilt / reorgs)
	pub fn non_best_imports(&self) -> usize {
		self.para_imports.iter().filter(|i| !i.is_best).count()
	}

	/// Block confidence: best / (best + non_best) as percentage
	pub fn block_confidence(&self) -> f64 {
		let best = self.best_imports() as f64;
		let total = self.para_imports.len() as f64;
		if total == 0.0 {
			return 100.0;
		}
		best / total * 100.0
	}
}

// ---- On-chain data types ----

#[derive(Clone, Debug)]
pub struct OnChainBlockInfo {
	pub block_number: u32,
	pub block_hash: [u8; 32],
	pub session_index: u32,
	pub backed_para_candidates: Vec<ParaCandidateEvent>,
	pub included_para_candidates: Vec<ParaCandidateEvent>,
	/// Core indices where our para is in the ClaimQueue at this relay block.
	/// Empty means the para was not scheduled on any core.
	pub claim_queue_cores: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct ParaCandidateEvent {
	pub para_id: u32,
	pub relay_parent: Option<[u8; 32]>,
	pub core_index: Option<u32>,
	pub group_index: Option<u32>,
	pub para_head_data: Option<Vec<u8>>,
}

// ---- Analysis types ----

#[derive(Clone, Debug)]
pub enum DropReason {
	SessionBoundary {
		session_before: u32,
		session_after: u32,
		boundary_relay_block: u32,
	},
	RelayParentExpired {
		relay_parent_num: u32,
		viability_window: u32,
	},
	WrongFork {
		relay_parent_num: u32,
		relay_parent_hash: LogHash,
	},
	Unknown,
}

impl fmt::Display for DropReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::SessionBoundary { session_before, session_after, boundary_relay_block } =>
				write!(
					f,
					"Session boundary (session {} → {} at relay block #{})",
					session_before, session_after, boundary_relay_block
				),
			Self::RelayParentExpired { relay_parent_num, viability_window } =>
				write!(
					f,
					"Relay parent expired (RP #{}, viability window {})",
					relay_parent_num, viability_window
				),
			Self::WrongFork { relay_parent_num, .. } =>
				write!(f, "Built on wrong fork (RP #{})", relay_parent_num),
			Self::Unknown => write!(f, "Unknown"),
		}
	}
}

/// A locally built block that was not included on-chain.
#[derive(Clone, Debug)]
pub struct DroppedBlock {
	pub para_block_number: u32,
	pub para_block_hash: LogHash,
	pub parent_hash: LogHash,
	pub relay_parent_num: u32,
	pub relay_parent_hash: LogHash,
	pub built_at: DateTime<Utc>,
	pub collation_fetch_latency_ms: Option<u64>,
	pub reason: DropReason,
	/// On-chain info for relay blocks near this drop
	pub nearby_relay_blocks: Vec<OnChainBlockInfo>,
	/// Matching "Collation expired" log entries for this block's relay parent
	pub collation_expired: Vec<CollationExpired>,
}

/// Aggregated analysis results.
pub struct Analysis {
	pub time_window: (DateTime<Utc>, DateTime<Utc>),
	pub total_built: usize,
	pub total_included: usize,
	pub total_dropped: usize,
	pub session_boundary_drops: Vec<DroppedBlock>,
	pub relay_parent_expired_drops: Vec<DroppedBlock>,
	pub wrong_fork_drops: Vec<DroppedBlock>,
	pub unknown_drops: Vec<DroppedBlock>,
	/// Blocks near the end of the log window that appear dropped but might have
	/// been included in relay blocks beyond what the log covers.
	pub edge_of_window_drops: Vec<DroppedBlock>,
	pub session_boundaries: Vec<(u32, u32, u32)>, // (relay_block, session_before, session_after)
	pub n_cores: u32,
}

impl Analysis {
	pub fn session_drop_stats(&self) -> (f64, u32, u32) {
		if self.session_boundaries.is_empty() {
			return (0.0, 0, 0);
		}
		let mut per_session: Vec<u32> = Vec::new();
		for &(boundary_block, _, _) in &self.session_boundaries {
			let count = self
				.session_boundary_drops
				.iter()
				.filter(|d| {
					let rp = d.relay_parent_num;
					rp >= boundary_block.saturating_sub(5) && rp <= boundary_block + 5
				})
				.count() as u32;
			per_session.push(count);
		}
		let total: u32 = per_session.iter().sum();
		let avg = total as f64 / per_session.len() as f64;
		let min = per_session.iter().copied().min().unwrap_or(0);
		let max = per_session.iter().copied().max().unwrap_or(0);
		(avg, min, max)
	}
}

// ---- Multi-collator types ----

/// Parsed logs from multiple collators.
pub struct MultiCollatorLogs {
	/// collator_name → ParsedLog
	pub collators: BTreeMap<String, ParsedLog>,
}

impl MultiCollatorLogs {
	/// Get an iterator over all collator names.
	pub fn collator_names(&self) -> impl Iterator<Item = &String> {
		self.collators.keys()
	}

	/// Compute the global relay block range across all collators.
	pub fn relay_block_range(&self) -> Option<(u32, u32)> {
		let min = self.collators.values().filter_map(|l| l.min_relay_block()).min()?;
		let max = self.collators.values().filter_map(|l| l.max_relay_block()).max()?;
		Some((min, max))
	}

	/// Compute the global time range across all collators.
	pub fn time_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
		let mut all_min = None;
		let mut all_max = None;
		for log in self.collators.values() {
			if let Some((min, max)) = log.time_range() {
				all_min = Some(all_min.map_or(min, |m: DateTime<Utc>| m.min(min)));
				all_max = Some(all_max.map_or(max, |m: DateTime<Utc>| m.max(max)));
			}
		}
		Some((all_min?, all_max?))
	}
}

/// A rebuild event where one collator's block was replaced by another's.
#[derive(Clone, Debug)]
pub struct RebuildEvent {
	/// The para block height where the rebuild occurred.
	pub block_number: u32,
	/// The 🏆 block that was originally best on the observing collator.
	pub block_hash_best: LogHash,
	/// Parent hash of the original best block.
	pub parent_hash_best: LogHash,
	/// The 🆕 block that replaced it.
	pub block_hash_rebuilt: LogHash,
	/// Parent hash of the replacement block.
	pub parent_hash_rebuilt: LogHash,
	/// Which collator built the original best block (from cross-referencing Pre-sealed events).
	pub best_collator: Option<String>,
	/// Which collator built the replacement block.
	pub rebuilt_collator: Option<String>,
	/// Timestamp of the original best import.
	pub best_timestamp: DateTime<Utc>,
	/// Timestamp of the rebuild import.
	pub rebuilt_timestamp: DateTime<Utc>,
	/// The collator that observed this rebuild.
	pub observer: String,
	/// Relay parent of the best block (if known from build attempts).
	pub best_relay_parent: Option<u32>,
	/// Relay parent of the rebuilt block (if known from build attempts).
	pub rebuilt_relay_parent: Option<u32>,
	/// Aura slot of the best block's builder.
	pub best_slot: Option<u64>,
	/// Aura slot of the rebuilt block's builder.
	pub rebuilt_slot: Option<u64>,
	/// Classified root cause.
	pub cause: RebuildCause,
	/// Which block was actually included on-chain at this height.
	pub on_chain_winner: OnChainWinner,
	/// Relay chain block sequence between the best and rebuilt relay parents.
	pub relay_block_sequence: Vec<OnChainBlockInfo>,
	/// "Collation expired" log entries matching the original (best) relay parent.
	/// Shows what happened to the tail collations on the old RP that never got backed.
	pub collation_expired: Vec<CollationExpired>,
}

/// Which block at a given height was included on-chain.
#[derive(Clone, Debug)]
pub enum OnChainWinner {
	/// The original 🏆 best block was the one included on-chain.
	OriginalBest,
	/// The 🆕 replacement block was the one included on-chain.
	Replacement,
	/// Neither block was found in the on-chain data (outside query range).
	Unknown,
}

/// Classified root cause of a rebuild.
#[derive(Clone, Debug)]
pub enum RebuildCause {
	/// Slot boundary timing overlap: the incoming collator started building
	/// before the outgoing collator's last block was propagated.
	SlotBoundaryOverlap {
		outgoing_slot: u64,
		incoming_slot: u64,
	},
	/// Same collator rebuilt on a newer relay parent in a later slot.
	NewerRelayParent {
		old_rp: u32,
		new_rp: u32,
	},
	/// Could not determine the cause.
	Unknown,
}

/// Per-collator authorship statistics.
///
/// Tracks how many blocks each collator authored (Pre-sealed) and how many
/// of those ended up as the 🏆 best block vs were replaced by a 🆕 rebuild.
#[derive(Clone, Debug)]
pub struct CollatorStats {
	pub name: String,
	/// Total blocks this collator authored (Pre-sealed events).
	pub blocks_built: usize,
	/// How many of this collator's blocks became the final 🏆 best.
	pub blocks_won: usize,
	/// How many of this collator's blocks were replaced by another block.
	pub blocks_lost: usize,
	/// Win rate: blocks_won / blocks_built.
	pub win_rate: f64,
}

/// A slot boundary transition between two collators.
#[derive(Clone, Debug)]
pub struct SlotBoundaryDetail {
	/// Aura slot of the outgoing collator.
	pub outgoing_slot: u64,
	/// Aura slot of the incoming collator.
	pub incoming_slot: u64,
	/// Name of the outgoing collator.
	pub outgoing_collator: String,
	/// Name of the incoming collator.
	pub incoming_collator: String,
	/// Time when the outgoing collator sealed its last block (Pre-sealed timestamp).
	pub outgoing_seal_time: DateTime<Utc>,
	/// Time when the incoming collator started building its first block.
	pub incoming_build_time: DateTime<Utc>,
	/// Propagation gap: incoming_build_time - outgoing_seal_time.
	/// Negative means the incoming started BEFORE the outgoing sealed (overlap).
	pub propagation_gap_ms: i64,
	/// Number of para block heights where both collators built a block.
	pub overlap_count: u32,
	/// Relay parent used by the outgoing collator.
	pub outgoing_rp: u32,
	/// Relay parent used by the incoming collator.
	pub incoming_rp: u32,
}

/// Aggregate slot boundary analysis.
#[derive(Clone, Debug)]
pub struct SlotBoundaryAnalysis {
	pub boundaries: Vec<SlotBoundaryDetail>,
	/// Number of boundaries with overlap (overlap_count > 0).
	pub boundaries_with_overlap: usize,
	/// Number of boundaries without overlap.
	pub boundaries_without_overlap: usize,
	/// Average propagation gap across all boundaries (ms).
	pub avg_propagation_gap_ms: f64,
	/// Median propagation gap (ms).
	pub median_propagation_gap_ms: i64,
	/// Min propagation gap (most negative = worst overlap).
	pub min_propagation_gap_ms: i64,
	/// Max propagation gap.
	pub max_propagation_gap_ms: i64,
	/// Average overlap count per boundary (for boundaries with overlap).
	pub avg_overlap_count: f64,
	/// Max overlap count at any single boundary.
	pub max_overlap_count: u32,
	/// Number of boundaries where the RP jumped by more than 1 (possible relay fork).
	pub relay_parent_gaps: usize,
}

/// A case where 2+ different collators built the same para block number
/// on the same relay parent. This would indicate a protocol-level issue
/// (Aura slot assignment violation or similar).
#[derive(Clone, Debug)]
pub struct DuplicateBlockOnSameRP {
	/// The para block height that was produced by multiple collators.
	pub block_number: u32,
	/// The relay parent number both collators built on.
	pub relay_parent_num: u32,
	/// Each collator that produced this block: (name, slot, timestamp).
	pub producers: Vec<DuplicateProducer>,
}

/// One producer in a duplicate block production event.
#[derive(Clone, Debug)]
pub struct DuplicateProducer {
	pub collator: String,
	pub slot: u64,
	pub timestamp: chrono::DateTime<chrono::Utc>,
	pub block_hash: LogHash,
}

/// Summary of rebuild analysis across all collators.
pub struct MultiCollatorAnalysis {
	pub per_collator: Vec<CollatorStats>,
	pub rebuilds: Vec<RebuildEvent>,
	pub slot_boundary_analysis: Option<SlotBoundaryAnalysis>,
	/// Cases where 2+ different collators built the same block number on the
	/// same relay parent. Empty means clean slot assignment (expected).
	pub duplicate_blocks_same_rp: Vec<DuplicateBlockOnSameRP>,
}

/// Inline hex module to avoid adding a dependency.
pub mod hex {
	pub fn encode(bytes: &[u8]) -> String {
		bytes.iter().map(|b| format!("{:02x}", b)).collect()
	}

	pub fn decode(s: &str) -> Result<Vec<u8>, String> {
		if s.len() % 2 != 0 {
			return Err("odd length".to_string());
		}
		(0..s.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
			.collect()
	}
}
