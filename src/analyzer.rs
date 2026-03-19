use crate::chain_client::ChainClient;
use crate::types::*;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Information about a locally built parachain block.
#[derive(Clone, Debug)]
pub struct BuiltBlock {
	pub block_number: u32,
	pub block_hash: LogHash,
	pub parent_hash: LogHash,
	pub relay_parent_num: u32,
	pub relay_parent_hash: LogHash,
	pub built_at: chrono::DateTime<chrono::Utc>,
	pub collation_fetch_latency_ms: Option<u64>,
	pub relay_parent_offset: Option<u32>,
}

/// Identify all blocks that were actually built and submitted by the collator.
/// Uses PreSealedBlock (has full hash + block number) correlated with
/// BuildAttempt (has relay parent context) and CollationSubmission events.
pub fn identify_built_blocks(log: &ParsedLog) -> Vec<BuiltBlock> {
	// Index collation fetch latencies by para_head hash
	let fetch_latency_by_hash: HashMap<String, u64> = log
		.collation_fetches
		.iter()
		.filter_map(|cf| {
			cf.para_head.full.map(|h| (format!("0x{}", hex::encode(&h)), cf.latency_ms))
		})
		.collect();

	// Index collation submissions by block number
	let submissions_by_number: HashMap<u32, &CollationSubmission> =
		log.collation_submissions.iter().map(|cs| (cs.block_number, cs)).collect();

	let mut built = Vec::new();

	for ps in &log.pre_sealed {
		// Find the matching BuildAttempt
		// Strategy: find a BuildAttempt(building=true) that is close in time and
		// where the parent hash matches
		let mut best_ba: Option<&BuildAttempt> = None;

		// Search build attempts near this pre-sealed event
		for ba in &log.build_attempts {
			if !ba.building {
				continue;
			}
			// Check time proximity (within 2 seconds)
			let dt = (ps.timestamp - ba.timestamp).num_milliseconds().unsigned_abs();
			if dt > 2000 {
				continue;
			}
			// For the BuildAttempt, the `parent` should match the pre-sealed block's parent
			// (which we don't directly have). But the build attempt's parent is the block
			// built upon, and pre-sealed block_number = parent_number + 1.
			// Use the submission to correlate by block number.
			if let Some(sub) = submissions_by_number.get(&ps.block_number) {
				// The submission hash should match the pre-sealed post_hash
				if let (Some(sub_full), Some(ps_full)) = (&sub.hash.full, &ps.post_hash.full) {
					if sub_full == ps_full {
						best_ba = Some(ba);
						break;
					}
				}
			}

			// Fallback: closest in time
			if best_ba.is_none() || {
				let prev_dt = best_ba
					.map(|b| (ps.timestamp - b.timestamp).num_milliseconds().unsigned_abs())
					.unwrap_or(u64::MAX);
				dt < prev_dt
			} {
				best_ba = Some(ba);
			}
		}

		let (rp_num, rp_hash, rp_offset) = if let Some(ba) = best_ba {
			(ba.relay_parent_num, ba.relay_parent.clone(), ba.relay_parent_offset)
		} else {
			// Try to get from collation submission's candidate generated event
			(0, LogHash::from_truncated("unknown"), None)
		};

		// Find fetch latency for this block hash
		let fetch_latency = ps
			.post_hash
			.full
			.and_then(|h| fetch_latency_by_hash.get(&format!("0x{}", hex::encode(&h))))
			.copied();

		built.push(BuiltBlock {
			block_number: ps.block_number,
			block_hash: ps.post_hash.clone(),
			parent_hash: LogHash::from_auto(""), // We'll fill this from import events
			relay_parent_num: rp_num,
			relay_parent_hash: rp_hash,
			built_at: ps.timestamp,
			collation_fetch_latency_ms: fetch_latency,
			relay_parent_offset: rp_offset,
		});
	}

	// Fill parent hashes from para import events
	let para_import_by_number: HashMap<u32, &ParaBlockImport> =
		log.para_imports.iter().map(|pi| (pi.block_number, pi)).collect();
	for bb in &mut built {
		if let Some(pi) = para_import_by_number.get(&bb.block_number) {
			bb.parent_hash = pi.parent_hash.clone();
		}
	}

	built.sort_by_key(|b| b.block_number);
	tracing::info!(count = built.len(), "Identified locally built blocks");
	built
}

/// Determine which built blocks were included on-chain and which were dropped.
pub async fn detect_and_classify_drops(
	built_blocks: &[BuiltBlock],
	log: &ParsedLog,
	client: &ChainClient,
	session_boundaries: &[(u32, u32, u32)],
	viability_window: u32,
) -> Result<Analysis> {
	let time_range = log.time_range().unwrap_or_default();

	if built_blocks.is_empty() {
		return Ok(Analysis {
			time_window: time_range,
			total_built: 0,
			total_included: 0,
			total_dropped: 0,
			session_boundary_drops: Vec::new(),
			relay_parent_expired_drops: Vec::new(),
			wrong_fork_drops: Vec::new(),
			unknown_drops: Vec::new(),
			edge_of_window_drops: Vec::new(),
			session_boundaries: session_boundaries.to_vec(),
			n_cores: detect_n_cores(log),
		});
	}

	// Determine the relay block range we need to query
	let min_rp = built_blocks.iter().map(|b| b.relay_parent_num).min().unwrap_or(0);
	let max_rp = built_blocks.iter().map(|b| b.relay_parent_num).max().unwrap_or(0);

	// Collect all relay blocks we need to query (near each built block's RP)
	let mut blocks_to_query: BTreeSet<u32> = BTreeSet::new();
	for bb in built_blocks {
		let rp = bb.relay_parent_num;
		for offset in 0..=(viability_window + 2) {
			blocks_to_query.insert(rp + offset);
		}
		// Also include the RP itself and one before
		if rp > 0 {
			blocks_to_query.insert(rp - 1);
		}
	}

	let blocks_vec: Vec<u32> = blocks_to_query.into_iter().collect();
	tracing::info!(
		count = blocks_vec.len(),
		range = format!("{}..{}", min_rp, max_rp + viability_window + 2),
		"Querying relay chain blocks for inclusion events"
	);

	let on_chain = client.query_relay_block_range(&blocks_vec).await?;

	// Build set of included para block numbers from on-chain events
	let mut included_para_blocks: BTreeSet<u32> = BTreeSet::new();
	let mut head_data_found = 0u32;
	let mut head_data_missing = 0u32;
	let mut decode_ok = 0u32;
	let mut decode_fail = 0u32;
	for info in on_chain.values() {
		for candidate in &info.included_para_candidates {
			if let Some(ref head_data) = candidate.para_head_data {
				head_data_found += 1;
				if let Some(num) = crate::chain_client::decode_para_block_number(head_data) {
					decode_ok += 1;
					included_para_blocks.insert(num);
				} else {
					decode_fail += 1;
				}
			} else {
				head_data_missing += 1;
			}
		}
	}

	tracing::info!(
		head_data_found,
		head_data_missing,
		decode_ok,
		decode_fail,
		unique_included = included_para_blocks.len(),
		"HeadData extraction stats for CandidateIncluded events"
	);
	if let (Some(min), Some(max)) = (included_para_blocks.iter().next(), included_para_blocks.iter().next_back()) {
		tracing::info!(min, max, "Included para block range");
	}

	// Also use backed events to know which blocks were at least backed
	let mut backed_para_blocks: BTreeSet<u32> = BTreeSet::new();
	for info in on_chain.values() {
		for candidate in &info.backed_para_candidates {
			if let Some(ref head_data) = candidate.para_head_data {
				if let Some(num) = crate::chain_client::decode_para_block_number(head_data) {
					backed_para_blocks.insert(num);
				}
			}
		}
	}

	// Detect drops: built locally but NOT included on-chain
	// Note: if we can't determine inclusion status (e.g., head_data decoding fails),
	// fall back to checking if the block's relay parent range had any inclusion for our para
	let mut dropped: Vec<DroppedBlock> = Vec::new();
	let mut included_count = 0;

	for bb in built_blocks {
		if included_para_blocks.contains(&bb.block_number) {
			included_count += 1;
			continue;
		}

		// This block was not included -> it's a drop
		let reason = classify_drop(
			bb,
			&on_chain,
			session_boundaries,
			viability_window,
			log,
		);

		// Gather nearby relay block info for the report
		let mut nearby: Vec<OnChainBlockInfo> = Vec::new();
		for offset in 0..=(viability_window + 2) {
			let block_num = bb.relay_parent_num + offset;
			if let Some(info) = on_chain.get(&block_num) {
				nearby.push(info.clone());
			}
		}

		dropped.push(DroppedBlock {
			para_block_number: bb.block_number,
			para_block_hash: bb.block_hash.clone(),
			parent_hash: bb.parent_hash.clone(),
			relay_parent_num: bb.relay_parent_num,
			relay_parent_hash: bb.relay_parent_hash.clone(),
			built_at: bb.built_at,
			collation_fetch_latency_ms: bb.collation_fetch_latency_ms,
			reason,
			nearby_relay_blocks: nearby,
		});
	}

	tracing::info!(
		total_built = built_blocks.len(),
		included = included_count,
		dropped = dropped.len(),
		"Drop detection complete"
	);

	// Separate edge-of-window drops: blocks whose relay parent is near the start
	// or end of the observed relay block range. These might have been included in
	// relay blocks outside what the log covers.
	let min_relay = log.min_relay_block().unwrap_or(0);
	let max_relay = log.max_relay_block().unwrap_or(0);
	let start_cutoff = min_relay + viability_window + 2;
	let end_cutoff = max_relay.saturating_sub(viability_window + 2);

	let mut real_drops = Vec::new();
	let mut edge_of_window_drops = Vec::new();
	for d in dropped {
		if d.relay_parent_num >= end_cutoff || d.relay_parent_num <= start_cutoff {
			edge_of_window_drops.push(d);
		} else {
			real_drops.push(d);
		}
	}

	if !edge_of_window_drops.is_empty() {
		tracing::info!(
			count = edge_of_window_drops.len(),
			start_cutoff,
			end_cutoff,
			"Excluded edge-of-window blocks (might have been included beyond log range)"
		);
	}

	// Classify real drops into categories
	let mut session_boundary_drops = Vec::new();
	let mut relay_parent_expired_drops = Vec::new();
	let mut wrong_fork_drops = Vec::new();
	let mut unknown_drops = Vec::new();

	for d in real_drops {
		match &d.reason {
			DropReason::SessionBoundary { .. } => session_boundary_drops.push(d),
			DropReason::RelayParentExpired { .. } => relay_parent_expired_drops.push(d),
			DropReason::WrongFork { .. } => wrong_fork_drops.push(d),
			DropReason::Unknown => unknown_drops.push(d),
		}
	}

	let total_dropped =
		session_boundary_drops.len() +
		relay_parent_expired_drops.len() +
		wrong_fork_drops.len() +
		unknown_drops.len();

	Ok(Analysis {
		time_window: time_range,
		total_built: built_blocks.len(),
		total_included: included_count,
		total_dropped,
		session_boundary_drops,
		relay_parent_expired_drops,
		wrong_fork_drops,
		unknown_drops,
		edge_of_window_drops,
		session_boundaries: session_boundaries.to_vec(),
		n_cores: detect_n_cores(log),
	})
}

fn classify_drop(
	bb: &BuiltBlock,
	on_chain: &BTreeMap<u32, OnChainBlockInfo>,
	session_boundaries: &[(u32, u32, u32)],
	viability_window: u32,
	log: &ParsedLog,
) -> DropReason {
	let rp = bb.relay_parent_num;

	// 1. Check for session boundary
	for &(boundary_block, session_before, session_after) in session_boundaries {
		// If the session boundary falls within the viability window of this block's RP
		if boundary_block > rp && boundary_block <= rp + viability_window + 1 {
			return DropReason::SessionBoundary {
				session_before,
				session_after,
				boundary_relay_block: boundary_block,
			};
		}
	}

	// 2. Check for wrong fork
	// Compare the relay parent hash from the log with the canonical hash from on-chain
	if let Some(on_chain_block) = on_chain.get(&rp) {
		let canonical_hash = on_chain_block.block_hash;
		if let Some(ref rp_full) = bb.relay_parent_hash.full {
			if rp_full != &canonical_hash {
				return DropReason::WrongFork {
					relay_parent_num: rp,
					relay_parent_hash: bb.relay_parent_hash.clone(),
				};
			}
		} else {
			// Truncated hash — try to match
			if !bb.relay_parent_hash.matches_full(&canonical_hash) {
				// Could be wrong fork or just can't match
				// Only classify as wrong fork if we're confident
				let resolved = log.hash_registry.resolve(&bb.relay_parent_hash.raw);
				if let Some(resolved_hash) = resolved {
					if resolved_hash != canonical_hash {
						return DropReason::WrongFork {
							relay_parent_num: rp,
							relay_parent_hash: bb.relay_parent_hash.clone(),
						};
					}
				}
			}
		}
	}

	// 3. Check for relay parent expired
	// Look at the relay blocks in the viability window: did any of them back our candidate?
	let mut any_backed = false;
	for offset in 1..=viability_window {
		if let Some(info) = on_chain.get(&(rp + offset)) {
			if !info.backed_para_candidates.is_empty() {
				any_backed = true;
				break;
			}
		}
	}

	if !any_backed {
		return DropReason::RelayParentExpired { relay_parent_num: rp, viability_window };
	}

	// If some blocks backed but the candidate still wasn't included, it might be
	// that a *different* candidate was backed (e.g., a competing fork).
	// Still classify as relay parent expired since the specific candidate expired.
	DropReason::RelayParentExpired { relay_parent_num: rp, viability_window }
}

/// Detect the number of cores from log data.
fn detect_n_cores(log: &ParsedLog) -> u32 {
	let unique_cores: BTreeSet<u32> =
		log.collation_submissions.iter().map(|cs| cs.core_index).collect();
	unique_cores.len().max(1) as u32
}

/// Analyze rebuilds across multiple collators.
///
/// For each 🆕 (non-best) import on a collator, cross-reference with other collators'
/// Pre-sealed events to determine who built the replacement block.
pub fn analyze_rebuilds(
	multi: &MultiCollatorLogs,
	_on_chain: &BTreeMap<u32, OnChainBlockInfo>,
) -> MultiCollatorAnalysis {
	// Build a merged hash registry from all collators so we can resolve
	// truncated hashes (from para imports like "0x4a22…16ba") to full hashes
	// (from Pre-sealed events).
	let mut merged_registry = HashRegistry::default();
	for log in multi.collators.values() {
		for (key, val) in &log.hash_registry.by_truncated {
			merged_registry.by_truncated.insert(key.clone(), *val);
		}
		for (key, val) in &log.hash_registry.by_full {
			merged_registry.by_full.insert(key.clone(), *val);
		}
	}

	// Build a lookup: (block_number, full_hash_hex) → collator name
	// from Pre-sealed events across all collators.
	// Pre-sealed events always have full hashes.
	let mut sealed_by: HashMap<(u32, String), String> = HashMap::new();
	for (name, log) in &multi.collators {
		for ps in &log.pre_sealed {
			let key = (ps.block_number, full_hash_key(&ps.post_hash));
			sealed_by.insert(key, name.clone());
		}
	}

	// Build a lookup: (collator_name, block_number) → (relay_parent_num, slot)
	// Match BuildAttempt to PreSealed by block number: for each PreSealed at height N,
	// find the BuildAttempt whose resulting block is at height N.
	// BuildAttempt at included_num + unincluded_segment_len + 1 = resulting block height.
	// Fall back to closest-before-in-time match at the same slot.
	let mut rp_and_slot_for_sealed: HashMap<(String, u32), (u32, u64)> = HashMap::new();
	for (name, log) in &multi.collators {
		let building_attempts: Vec<&BuildAttempt> =
			log.build_attempts.iter().filter(|ba| ba.building).collect();

		for ps in &log.pre_sealed {
			let target_height = ps.block_number;

			// Strategy 1: Find a BuildAttempt that produces this block height,
			// closest in time and BEFORE the pre-seal.
			let mut best_ba: Option<&BuildAttempt> = None;
			let mut best_dt = i64::MAX;
			for ba in &building_attempts {
				// The block being built is at: included_num + unincluded_segment_len + 1
				let ba_height = ba.included_num + ba.unincluded_segment_len + 1;
				if ba_height != target_height {
					continue;
				}
				let dt = (ps.timestamp - ba.timestamp).num_milliseconds();
				// Must be before or at the pre-seal (dt >= 0) and within 5s
				if dt >= 0 && dt < 5000 && dt < best_dt {
					best_dt = dt;
					best_ba = Some(ba);
				}
			}

			// Strategy 2: If no height match, find closest building attempt before
			// the pre-seal at the same slot.
			if best_ba.is_none() {
				for ba in &building_attempts {
					let dt = (ps.timestamp - ba.timestamp).num_milliseconds();
					if dt >= 0 && dt < 2000 {
						if best_ba.is_none() || dt < best_dt {
							best_dt = dt;
							best_ba = Some(ba);
						}
					}
				}
			}

			if let Some(ba) = best_ba {
				rp_and_slot_for_sealed
					.insert((name.clone(), ps.block_number), (ba.relay_parent_num, ba.slot));
			}
		}
	}

	let sealed_by_count = sealed_by.len();
	let rp_count = rp_and_slot_for_sealed.len();
	tracing::info!(
		sealed_by_entries = sealed_by_count,
		rp_slot_entries = rp_count,
		merged_registry_truncated = merged_registry.by_truncated.len(),
		merged_registry_full = merged_registry.by_full.len(),
		"Multi-collator lookup tables built"
	);

	// Build per-collator authorship stats.
	// For each para block height, determine which collator's block is the final 🏆 best.
	// Use the first collator's import log as reference (all nodes see the same chain).
	let per_collator = {
		let mut won_count: HashMap<String, usize> = HashMap::new();
		let mut lost_count: HashMap<String, usize> = HashMap::new();
		let built_count: HashMap<String, usize> = multi
			.collators
			.iter()
			.map(|(name, log)| (name.clone(), log.pre_sealed.len()))
			.collect();

		// Use any collator's para imports to find the final best at each height.
		// The last 🏆 import at a given height is the final best.
		if let Some(ref_log) = multi.collators.values().next() {
			let mut final_best_at: BTreeMap<u32, String> = BTreeMap::new();
			for pi in &ref_log.para_imports {
				if pi.is_best {
					let resolved = resolve_hash_key(&pi.block_hash, &merged_registry);
					final_best_at.insert(pi.block_number, resolved);
				}
			}

			// For each height with a final best, attribute wins/losses
			for (block_number, best_hash) in &final_best_at {
				let best_key = (*block_number, best_hash.clone());
				if let Some(winner) = sealed_by.get(&best_key) {
					*won_count.entry(winner.clone()).or_default() += 1;
				}

				// Any other collator that sealed a block at this height lost
				for (name, log) in &multi.collators {
					for ps in &log.pre_sealed {
						if ps.block_number == *block_number {
							let ps_key = full_hash_key(&ps.post_hash);
							if &ps_key != best_hash {
								*lost_count.entry(name.clone()).or_default() += 1;
							}
						}
					}
				}
			}
		}

		multi
			.collators
			.keys()
			.map(|name| {
				let built = *built_count.get(name).unwrap_or(&0);
				let won = *won_count.get(name).unwrap_or(&0);
				let lost = *lost_count.get(name).unwrap_or(&0);
				let win_rate = if built > 0 { won as f64 / built as f64 * 100.0 } else { 0.0 };
				CollatorStats {
					name: name.clone(),
					blocks_built: built,
					blocks_won: won,
					blocks_lost: lost,
					win_rate,
				}
			})
			.collect::<Vec<_>>()
	};

	// Collect rebuild events
	let mut rebuilds = Vec::new();

	for (observer_name, log) in &multi.collators {
		// Group para imports by block number
		let mut by_number: BTreeMap<u32, Vec<&ParaBlockImport>> = BTreeMap::new();
		for pi in &log.para_imports {
			by_number.entry(pi.block_number).or_default().push(pi);
		}

		for (block_number, imports) in &by_number {
			let best_imports: Vec<&&ParaBlockImport> =
				imports.iter().filter(|i| i.is_best).collect();
			let non_best_imports: Vec<&&ParaBlockImport> =
				imports.iter().filter(|i| !i.is_best).collect();

			if non_best_imports.is_empty() || best_imports.is_empty() {
				continue;
			}

			// For each 🆕 import, find which collator built the replacement
			let best = best_imports[0];

			for rebuilt in &non_best_imports {
				// Resolve truncated hashes to full using the merged registry
				let best_key =
					(*block_number, resolve_hash_key(&best.block_hash, &merged_registry));
				let rebuilt_key =
					(*block_number, resolve_hash_key(&rebuilt.block_hash, &merged_registry));

				let best_collator = sealed_by.get(&best_key).cloned();
				let rebuilt_collator = sealed_by.get(&rebuilt_key).cloned();

				let best_rp_slot = best_collator.as_ref().and_then(|c| {
					rp_and_slot_for_sealed.get(&(c.clone(), *block_number)).copied()
				});
				let rebuilt_rp_slot = rebuilt_collator.as_ref().and_then(|c| {
					rp_and_slot_for_sealed.get(&(c.clone(), *block_number)).copied()
				});

				let best_rp = best_rp_slot.map(|(rp, _)| rp);
				let rebuilt_rp = rebuilt_rp_slot.map(|(rp, _)| rp);
				let best_slot = best_rp_slot.map(|(_, s)| s);
				let rebuilt_slot = rebuilt_rp_slot.map(|(_, s)| s);

				// Classify the cause
				let cause = classify_rebuild_cause(
					&best_collator,
					&rebuilt_collator,
					best_rp,
					rebuilt_rp,
					best_slot,
					rebuilt_slot,
				);

				rebuilds.push(RebuildEvent {
					block_number: *block_number,
					block_hash_best: best.block_hash.clone(),
					block_hash_rebuilt: rebuilt.block_hash.clone(),
					best_collator,
					rebuilt_collator,
					best_timestamp: best.timestamp,
					rebuilt_timestamp: rebuilt.timestamp,
					observer: observer_name.clone(),
					best_relay_parent: best_rp,
					rebuilt_relay_parent: rebuilt_rp,
					best_slot,
					rebuilt_slot,
					cause,
				});
			}
		}
	}

	// Deduplicate rebuilds by (block_number, best_hash, rebuilt_hash) keeping first observer
	rebuilds.sort_by(|a, b| a.block_number.cmp(&b.block_number));
	let mut seen: BTreeSet<(u32, String, String)> = BTreeSet::new();
	rebuilds.retain(|r| {
		let best_key = resolve_hash_key(&r.block_hash_best, &merged_registry);
		let rebuilt_key = resolve_hash_key(&r.block_hash_rebuilt, &merged_registry);
		seen.insert((r.block_number, best_key, rebuilt_key))
	});

	let attributed = rebuilds.iter().filter(|r| r.best_collator.is_some()).count();
	tracing::info!(
		total_rebuilds = rebuilds.len(),
		attributed,
		unattributed = rebuilds.len() - attributed,
		"Rebuild attribution results"
	);

	// Slot boundary analysis
	let slot_boundary_analysis = Some(analyze_slot_boundaries(multi));

	MultiCollatorAnalysis { per_collator, rebuilds, slot_boundary_analysis }
}

/// Analyze slot boundary transitions across all collators.
///
/// For each consecutive pair of slots (from different collators), measure:
/// 1. Propagation delay: time between outgoing seal and incoming build start
/// 2. Overlap size: how many heights both collators built at
/// 3. Relay parent gap: whether the RP jumped by more than 1 (possible relay fork)
pub fn analyze_slot_boundaries(multi: &MultiCollatorLogs) -> SlotBoundaryAnalysis {
	// Collect all (slot, collator_name, rp, first_build_time, last_seal_time, heights_built)
	// from each collator's building bursts.
	struct SlotBurst {
		slot: u64,
		collator: String,
		rp: u32,
		first_build_time: chrono::DateTime<chrono::Utc>,
		last_seal_time: chrono::DateTime<chrono::Utc>,
		heights: BTreeSet<u32>,
	}

	let mut bursts: Vec<SlotBurst> = Vec::new();

	for (name, log) in &multi.collators {
		// Group building attempts by slot
		let mut by_slot: BTreeMap<u64, Vec<&BuildAttempt>> = BTreeMap::new();
		for ba in &log.build_attempts {
			if ba.building {
				by_slot.entry(ba.slot).or_default().push(ba);
			}
		}

		// Group pre-sealed by slot (match via time proximity to building attempts)
		let mut sealed_by_slot: BTreeMap<u64, Vec<&PreSealedBlock>> = BTreeMap::new();
		for ps in &log.pre_sealed {
			// Find which slot this pre-sealed belongs to
			let mut best_slot = None;
			let mut best_dt = i64::MAX;
			for (&slot, bas) in &by_slot {
				for ba in bas {
					let dt = (ps.timestamp - ba.timestamp).num_milliseconds();
					if dt >= 0 && dt < 5000 && dt < best_dt {
						best_dt = dt;
						best_slot = Some(slot);
					}
				}
			}
			if let Some(slot) = best_slot {
				sealed_by_slot.entry(slot).or_default().push(ps);
			}
		}

		for (&slot, bas) in &by_slot {
			if bas.is_empty() {
				continue;
			}
			let rp = bas[0].relay_parent_num;
			let first_build_time = bas.iter().map(|ba| ba.timestamp).min().unwrap();

			// Get heights built
			let heights: BTreeSet<u32> = bas
				.iter()
				.map(|ba| ba.included_num + ba.unincluded_segment_len + 1)
				.collect();

			// Get last seal time
			let last_seal_time = sealed_by_slot
				.get(&slot)
				.and_then(|seals| seals.iter().map(|ps| ps.timestamp).max())
				.unwrap_or(first_build_time);

			bursts.push(SlotBurst {
				slot,
				collator: name.clone(),
				rp,
				first_build_time,
				last_seal_time,
				heights,
			});
		}
	}

	// Sort by slot
	bursts.sort_by_key(|b| b.slot);

	// Deduplicate: keep one burst per slot (different collators claim different slots)
	// If two collators claim the same slot, that's unexpected — keep the first.
	let mut seen_slots: BTreeSet<u64> = BTreeSet::new();
	bursts.retain(|b| seen_slots.insert(b.slot));

	// Analyze consecutive pairs
	let mut boundaries = Vec::new();
	for window in bursts.windows(2) {
		let outgoing = &window[0];
		let incoming = &window[1];

		// Only analyze transitions between different collators
		if outgoing.collator == incoming.collator {
			continue;
		}

		let propagation_gap_ms =
			(incoming.first_build_time - outgoing.last_seal_time).num_milliseconds();

		let overlap_count = outgoing
			.heights
			.intersection(&incoming.heights)
			.count() as u32;

		boundaries.push(SlotBoundaryDetail {
			outgoing_slot: outgoing.slot,
			incoming_slot: incoming.slot,
			outgoing_collator: outgoing.collator.clone(),
			incoming_collator: incoming.collator.clone(),
			outgoing_seal_time: outgoing.last_seal_time,
			incoming_build_time: incoming.first_build_time,
			propagation_gap_ms,
			overlap_count,
			outgoing_rp: outgoing.rp,
			incoming_rp: incoming.rp,
		});
	}

	// Compute aggregates
	let boundaries_with_overlap = boundaries.iter().filter(|b| b.overlap_count > 0).count();
	let boundaries_without_overlap = boundaries.len() - boundaries_with_overlap;

	let mut gaps: Vec<i64> = boundaries.iter().map(|b| b.propagation_gap_ms).collect();
	gaps.sort();

	let avg_propagation_gap_ms = if gaps.is_empty() {
		0.0
	} else {
		gaps.iter().sum::<i64>() as f64 / gaps.len() as f64
	};

	let median_propagation_gap_ms = if gaps.is_empty() {
		0
	} else {
		gaps[gaps.len() / 2]
	};

	let min_propagation_gap_ms = gaps.first().copied().unwrap_or(0);
	let max_propagation_gap_ms = gaps.last().copied().unwrap_or(0);

	let overlapping: Vec<&SlotBoundaryDetail> =
		boundaries.iter().filter(|b| b.overlap_count > 0).collect();
	let avg_overlap_count = if overlapping.is_empty() {
		0.0
	} else {
		overlapping.iter().map(|b| b.overlap_count as f64).sum::<f64>() / overlapping.len() as f64
	};
	let max_overlap_count = boundaries.iter().map(|b| b.overlap_count).max().unwrap_or(0);

	// Detect relay parent gaps > 1 between consecutive slots
	let relay_parent_gaps = boundaries
		.iter()
		.filter(|b| {
			let rp_diff = (b.incoming_rp as i64 - b.outgoing_rp as i64).unsigned_abs();
			rp_diff > 1
		})
		.count();

	tracing::info!(
		total_boundaries = boundaries.len(),
		with_overlap = boundaries_with_overlap,
		without_overlap = boundaries_without_overlap,
		avg_gap_ms = format!("{:.0}", avg_propagation_gap_ms),
		median_gap_ms = median_propagation_gap_ms,
		min_gap_ms = min_propagation_gap_ms,
		relay_parent_gaps,
		"Slot boundary analysis complete"
	);

	SlotBoundaryAnalysis {
		boundaries,
		boundaries_with_overlap,
		boundaries_without_overlap,
		avg_propagation_gap_ms,
		median_propagation_gap_ms,
		min_propagation_gap_ms,
		max_propagation_gap_ms,
		avg_overlap_count,
		max_overlap_count,
		relay_parent_gaps,
	}
}

/// Classify the root cause of a rebuild.
fn classify_rebuild_cause(
	best_collator: &Option<String>,
	rebuilt_collator: &Option<String>,
	best_rp: Option<u32>,
	rebuilt_rp: Option<u32>,
	best_slot: Option<u64>,
	rebuilt_slot: Option<u64>,
) -> RebuildCause {
	match (best_slot, rebuilt_slot) {
		(Some(bs), Some(rs)) if bs != rs => {
			// Different slots = different collators in round-robin.
			// This is a slot boundary overlap: the incoming collator started
			// building before the outgoing collator's block was propagated.
			RebuildCause::SlotBoundaryOverlap {
				outgoing_slot: bs.min(rs),
				incoming_slot: bs.max(rs),
			}
		},
		(Some(_), Some(_)) => {
			// Same slot but different blocks at the same height.
			// This happens when the same collator rebuilds on a newer RP
			// in its next turn (the slot numbers wrapped around).
			// Actually check RPs to distinguish.
			match (best_rp, rebuilt_rp) {
				(Some(brp), Some(rrp)) if brp != rrp => RebuildCause::NewerRelayParent {
					old_rp: brp.min(rrp),
					new_rp: brp.max(rrp),
				},
				_ => RebuildCause::Unknown,
			}
		},
		// If we have RPs but no slots, fall back to RP-based classification
		(_, _) => match (best_rp, rebuilt_rp) {
			(Some(brp), Some(rrp)) if brp != rrp => {
				// Different RPs from different collators is most likely
				// a slot boundary overlap
				if best_collator != rebuilt_collator &&
					best_collator.is_some() &&
					rebuilt_collator.is_some()
				{
					// Approximate slot from RP difference
					RebuildCause::SlotBoundaryOverlap {
						outgoing_slot: 0,
						incoming_slot: 0,
					}
				} else {
					RebuildCause::NewerRelayParent { old_rp: brp.min(rrp), new_rp: brp.max(rrp) }
				}
			},
			_ => RebuildCause::Unknown,
		},
	}
}

/// Get the full hex key from a LogHash. Always returns the full form if available.
fn full_hash_key(h: &LogHash) -> String {
	if let Some(ref full) = h.full {
		format!("0x{}", hex::encode(full))
	} else {
		h.raw.clone()
	}
}

/// Resolve a (possibly truncated) LogHash to its full hex key using the registry.
/// Falls back to raw string if resolution fails.
fn resolve_hash_key(h: &LogHash, registry: &HashRegistry) -> String {
	// If we already have the full hash, use it
	if let Some(ref full) = h.full {
		return format!("0x{}", hex::encode(full));
	}
	// Try resolving the truncated hash via the registry
	if let Some(full) = registry.resolve(&h.raw) {
		return format!("0x{}", hex::encode(&full));
	}
	// Fallback: return raw (won't match, but at least we tried)
	h.raw.clone()
}
