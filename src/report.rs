use crate::types::*;
use std::fmt::Write;

pub struct ReportConfig {
	pub chain_name: String,
	pub max_samples: usize,
	#[allow(dead_code)]
	pub viability_window: u32,
	pub para_id: u32,
	#[allow(dead_code)]
	pub rpc_url: String,
	pub n_collators: usize,
}

pub fn generate_report(
	analysis: &Analysis,
	config: &ReportConfig,
	log: &ParsedLog,
) -> String {
	let mut out = String::with_capacity(8192);

	write_confidence(&mut out, log);
	write_summary(&mut out, analysis, config);
	write_session_boundary_section(&mut out, analysis, config);
	write_non_session_section(&mut out, analysis, config);
	write_sample_data_points(&mut out, analysis, config);

	out
}

pub fn generate_multi_collator_report(
	analysis: &Analysis,
	config: &ReportConfig,
	multi_analysis: &MultiCollatorAnalysis,
	primary_log: &ParsedLog,
) -> String {
	let mut out = String::with_capacity(16384);

	// Use the primary (first) collator's confidence for the main table
	write_confidence(&mut out, primary_log);
	write_multi_collator_section(&mut out, multi_analysis, config);
	write_duplicate_block_section(&mut out, multi_analysis);
	write_summary(&mut out, analysis, config);
	write_session_boundary_section(&mut out, analysis, config);
	write_non_session_section(&mut out, analysis, config);
	write_sample_data_points(&mut out, analysis, config);

	out
}

fn write_confidence(out: &mut String, log: &ParsedLog) {
	let best = log.best_imports();
	let rebuilt = log.non_best_imports();
	let total = best + rebuilt;
	let confidence = log.block_confidence();

	let _ = writeln!(out, "## Block Confidence\n");
	let _ = writeln!(out, "| Metric | Value |");
	let _ = writeln!(out, "|---|---|");
	let _ = writeln!(out, "| \u{1f3c6} Blocks built (best) | **{}** |", best);
	let _ = writeln!(out, "| \u{1f195} Blocks rebuilt (non-best) | **{}** |", rebuilt);
	let _ = writeln!(out, "| Total parachain imports | {} |", total);
	let _ = writeln!(out, "| **Block Confidence** | **{:.2}%** |", confidence);
	if best > 0 {
		let rebuild_rate = rebuilt as f64 / best as f64 * 100.0;
		let _ = writeln!(out, "| Rebuild rate | {:.2}% |", rebuild_rate);
	}
	let _ = writeln!(out);
}

fn write_summary(out: &mut String, analysis: &Analysis, config: &ReportConfig) {
	let (start, end) = analysis.time_window;
	let duration = end - start;
	let duration_str = format_duration(duration);

	let _ = writeln!(out, "## Summary of top issues identified over a {} window\n", duration_str);
	let _ = writeln!(
		out,
		"This data was collected from collator logs for para_id {}.",
		config.para_id
	);
	let _ = writeln!(out);
	let _ = writeln!(
		out,
		"Setup: {} collator(s), {} core(s).\n",
		config.n_collators, analysis.n_cores
	);
	let _ = writeln!(
		out,
		"- Total blocks built locally: **{}**",
		analysis.total_built
	);
	let _ = writeln!(
		out,
		"- Total blocks included on-chain: **{}**",
		analysis.total_included
	);
	let _ = writeln!(
		out,
		"- Total blocks dropped: **{}**",
		analysis.total_dropped
	);
	if !analysis.edge_of_window_drops.is_empty() {
		let _ = writeln!(
			out,
			"- Edge-of-window blocks excluded: **{}** (these blocks were built near the \
			start/end of the log window and might have been included in relay chain blocks \
			outside the observed range)",
			analysis.edge_of_window_drops.len()
		);
	}
	let _ = writeln!(out);
}

fn write_session_boundary_section(out: &mut String, analysis: &Analysis, config: &ReportConfig) {
	let session_drops = analysis.session_boundary_drops.len();
	let (avg, min, max) = analysis.session_drop_stats();

	let session_duration = if analysis.session_boundaries.len() >= 2 {
		let first = analysis.session_boundaries.first().unwrap().0;
		let last = analysis.session_boundaries.last().unwrap().0;
		let blocks = last - first;
		let sessions = analysis.session_boundaries.len() as u32 - 1;
		if sessions > 0 {
			let blocks_per_session = blocks / sessions;
			format!("~{} blocks (~{} minutes)", blocks_per_session, blocks_per_session * 6 / 60)
		} else {
			"unknown".to_string()
		}
	} else {
		"unknown".to_string()
	};

	let _ = writeln!(
		out,
		"### Session boundaries on {} (every {})\n",
		config.chain_name, session_duration
	);

	if session_drops == 0 && analysis.session_boundaries.is_empty() {
		let _ = writeln!(
			out,
			"No session boundaries observed in the log window. Cannot assess session boundary drops.\n"
		);
	} else if session_drops == 0 {
		let _ = writeln!(
			out,
			"No blocks dropped at session boundaries ({} session change(s) observed).\n",
			analysis.session_boundaries.len()
		);
	} else {
		let _ = writeln!(
			out,
			"Analysis shows a total of **{} blocks** being dropped at session boundaries.\n",
			session_drops
		);
		let _ = writeln!(out, "- Average (per session change): **{:.1} blocks**", avg);
		let _ = writeln!(out, "- Min: **{} block(s)**", min);
		let _ = writeln!(out, "- Max: **{} blocks**", max);
		let _ = writeln!(out);
	}
}

fn write_non_session_section(out: &mut String, analysis: &Analysis, _config: &ReportConfig) {
	let rp_expired = analysis.relay_parent_expired_drops.len();
	let wrong_fork = analysis.wrong_fork_drops.len();
	let unknown = analysis.unknown_drops.len();
	let total = rp_expired + wrong_fork + unknown;

	let _ = writeln!(out, "### Non-session boundary\n");

	if total == 0 {
		let _ = writeln!(out, "No blocks dropped outside of session boundaries.\n");
		return;
	}

	let _ = writeln!(
		out,
		"Analysis shows a total of **{} blocks** being dropped.\n",
		total
	);
	let _ = writeln!(out, "Approximate breakdown:");

	if total > 0 {
		let rp_pct = rp_expired as f64 / total as f64 * 100.0;
		let fork_pct = wrong_fork as f64 / total as f64 * 100.0;
		let _ = writeln!(
			out,
			"- Relay parent expired (**~{:.0}%**). Why?",
			rp_pct
		);
		let _ = writeln!(
			out,
			"\t- Low performance validators produce empty relay blocks or blocks with very few backed candidates"
		);
		let _ = writeln!(
			out,
			"- Built on wrong fork (~**{:.0}%**)",
			fork_pct
		);
	}

	if unknown > 0 {
		let _ = writeln!(out, "- Unknown reason: **{}**", unknown);
	}

	let _ = writeln!(out);
}

fn write_sample_data_points(out: &mut String, analysis: &Analysis, config: &ReportConfig) {
	let _ = writeln!(out, "## Sample data points");

	// Session boundary drops
	if !analysis.session_boundary_drops.is_empty() {
		let _ = writeln!(out, "### Session boundary drops");
		let samples = &analysis.session_boundary_drops[..analysis
			.session_boundary_drops
			.len()
			.min(config.max_samples)];
		for drop in samples {
			write_drop_sample(out, drop, config);
		}
	}

	// Relay parent expired drops
	if !analysis.relay_parent_expired_drops.is_empty() {
		let _ = writeln!(out, "### Relay parent expired drops");
		let samples = &analysis.relay_parent_expired_drops[..analysis
			.relay_parent_expired_drops
			.len()
			.min(config.max_samples)];
		for drop in samples {
			write_drop_sample(out, drop, config);
		}
	}

	// Wrong fork drops
	if !analysis.wrong_fork_drops.is_empty() {
		let _ = writeln!(out, "### Blocks built on wrong fork");
		let samples = &analysis.wrong_fork_drops
			[..analysis.wrong_fork_drops.len().min(config.max_samples)];
		for drop in samples {
			write_drop_sample(out, drop, config);
		}
	}
}

fn write_drop_sample(out: &mut String, drop: &DroppedBlock, config: &ReportConfig) {
	let _ = writeln!(
		out,
		"#### \u{1f3c6} Imported #{} ({} → {})",
		drop.para_block_number,
		drop.parent_hash.short(),
		drop.para_block_hash.short()
	);

	// Root cause
	let _ = writeln!(out, "- **Root cause**: {}", drop.reason);

	// Timing
	let _ = writeln!(out, "- Built at: {}", drop.built_at.format("%H:%M:%S%.3f"));
	if let Some(latency) = drop.collation_fetch_latency_ms {
		let _ = writeln!(out, "- Collation fetch latency: **{}ms**", latency);
	}

	// Relay parent info
	let _ = writeln!(
		out,
		"- Relay parent: #{} ({})",
		drop.relay_parent_num, drop.relay_parent_hash.short()
	);

	// Check if RP is on wrong fork
	match &drop.reason {
		DropReason::WrongFork { .. } => {
			let _ = writeln!(out, "- RP **on wrong fork**: relay parent was pruned from canonical chain");
		},
		_ => {
			let _ = writeln!(out, "- RP is not on wrong fork");
		},
	}

	// Relay chain block sequence
	if !drop.nearby_relay_blocks.is_empty() {
		let _ = writeln!(out, "- Relay chain block sequence:");
		for info in &drop.nearby_relay_blocks {
			let hash_short = format!("0x{}…{}", &hex::encode(&info.block_hash[..2]), &hex::encode(&info.block_hash[30..32]));

			let backed_info = if info.backed_para_candidates.is_empty() {
				"no backed candidates for our para".to_string()
			} else {
				let mut groups: Vec<u32> = info.backed_para_candidates.iter()
					.filter_map(|c| c.group_index)
					.collect();
				groups.sort();
				groups.dedup();
				let group_note = if groups.is_empty() {
					String::new()
				} else {
					let g: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
					format!(" (group(s) {})", g.join(", "))
				};
				format!(
					"backs {} candidate(s) for para {}{}",
					info.backed_para_candidates.len(),
					config.para_id,
					group_note,
				)
			};

			let included_info = if info.included_para_candidates.is_empty() {
				String::new()
			} else {
				format!(
					", includes {} candidate(s)",
					info.included_para_candidates.len()
				)
			};

			let session_note = if drop.nearby_relay_blocks.len() > 1 {
				// Check if session changed from previous block
				let prev = drop
					.nearby_relay_blocks
					.iter()
					.find(|b| b.block_number == info.block_number.saturating_sub(1));
				if let Some(prev) = prev {
					if prev.session_index != info.session_index {
						format!(", **NEW SESSION** ({} → {})", prev.session_index, info.session_index)
					} else {
						String::new()
					}
				} else {
					String::new()
				}
			} else {
				String::new()
			};

			let schedule_note = if info.claim_queue_cores.is_empty() {
				" — **not scheduled**".to_string()
			} else {
				let cores: Vec<String> = info.claim_queue_cores.iter().map(|c| c.to_string()).collect();
				format!(" — scheduled on core(s) {}", cores.join(", "))
			};

			let _ = writeln!(
				out,
				"  - #{} ({}): {}{}{}{}",
				info.block_number,
				hash_short,
				backed_info,
				included_info,
				session_note,
				schedule_note,
			);
		}
	}

	// Collation expiry info
	if !drop.collation_expired.is_empty() {
		// Group by collation_state and count
		let mut state_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
		for ce in &drop.collation_expired {
			*state_counts.entry(&ce.collation_state).or_insert(0) += 1;
		}
		let summary: Vec<String> = state_counts.iter()
			.map(|(state, count)| format!("{} x {}", count, state))
			.collect();
		let _ = writeln!(
			out,
			"- Collation expiry on RP #{}: **{}** collation(s) expired — {}",
			drop.relay_parent_num,
			drop.collation_expired.len(),
			summary.join(", "),
		);
		// Show individual entries if there are few enough
		if drop.collation_expired.len() <= 20 {
			for ce in &drop.collation_expired {
				let age_str = ce.age.map_or(String::new(), |a| format!(", age={}", a));
				let head_str = ce.head.as_ref().map_or(String::new(), |h| format!(", head={}", h.short()));
				let _ = writeln!(
					out,
					"  - state=**{}**{}{} ({})",
					ce.collation_state,
					age_str,
					head_str,
					ce.timestamp.format("%H:%M:%S%.3f"),
				);
			}
		}
	}

	let _ = writeln!(out);
}

fn write_multi_collator_section(
	out: &mut String,
	multi: &MultiCollatorAnalysis,
	config: &ReportConfig,
) {
	let _ = writeln!(out, "## Multi-Collator Rebuild Analysis\n");

	// Per-collator authorship table
	let _ = writeln!(
		out,
		"| Collator | Blocks Built | \u{1f3c6} Won (final best) | Lost (replaced) | Win Rate |"
	);
	let _ = writeln!(out, "|---|---|---|---|---|");
	for cs in &multi.per_collator {
		let _ = writeln!(
			out,
			"| {} | {} | {} | {} | {:.2}% |",
			cs.name, cs.blocks_built, cs.blocks_won, cs.blocks_lost, cs.win_rate
		);
	}
	let _ = writeln!(out);

	if multi.rebuilds.is_empty() {
		let _ = writeln!(out, "No rebuild events detected across collators.\n");
		return;
	}

	// Classify rebuilds using the pre-computed cause
	let total = multi.rebuilds.len();
	let mut slot_boundary = 0usize;
	let mut newer_rp = 0usize;
	let mut unknown = 0usize;

	for r in &multi.rebuilds {
		match &r.cause {
			RebuildCause::SlotBoundaryOverlap { .. } => slot_boundary += 1,
			RebuildCause::NewerRelayParent { .. } => newer_rp += 1,
			RebuildCause::Unknown => unknown += 1,
		}
	}

	let pct = |n: usize| n as f64 / total as f64 * 100.0;
	let _ = writeln!(out, "### Rebuild Breakdown ({} total)\n", total);
	if slot_boundary > 0 {
		let _ = writeln!(
			out,
			"- Slot boundary overlap: **{}** ({:.0}%) — incoming collator started before outgoing collator's block propagated",
			slot_boundary, pct(slot_boundary)
		);
	}
	if newer_rp > 0 {
		let _ = writeln!(
			out,
			"- Rebuilt on newer relay parent: **{}** ({:.0}%) — collator rebuilt at same height on a newer RP in a later slot",
			newer_rp, pct(newer_rp)
		);
	}
	if unknown > 0 {
		let _ = writeln!(
			out,
			"- Unknown: **{}** ({:.0}%)",
			unknown, pct(unknown)
		);
	}
	let _ = writeln!(out);

	// On-chain winner analysis
	let mut winner_original = 0usize;
	let mut winner_replacement = 0usize;
	let mut winner_unknown = 0usize;
	for r in &multi.rebuilds {
		match &r.on_chain_winner {
			OnChainWinner::OriginalBest => winner_original += 1,
			OnChainWinner::Replacement => winner_replacement += 1,
			OnChainWinner::Unknown => winner_unknown += 1,
		}
	}

	let _ = writeln!(out, "### On-Chain Inclusion Outcome\n");
	let _ = writeln!(
		out,
		"For each rebuild, which block was actually included on-chain?\n"
	);
	let _ = writeln!(out, "| Outcome | Count | % |");
	let _ = writeln!(out, "|---|---|---|");
	let _ = writeln!(
		out,
		"| Original best (\u{1f3c6}) won on-chain | {} | {:.0}% |",
		winner_original, pct(winner_original)
	);
	let _ = writeln!(
		out,
		"| Replacement (\u{1f195}) won on-chain | {} | {:.0}% |",
		winner_replacement, pct(winner_replacement)
	);
	if winner_unknown > 0 {
		let _ = writeln!(
			out,
			"| Unknown (outside query range) | {} | {:.0}% |",
			winner_unknown, pct(winner_unknown)
		);
	}
	let _ = writeln!(out);

	if winner_replacement > 0 {
		let _ = writeln!(
			out,
			"**{}** rebuilds ({:.0}%) resulted in the replacement block winning on-chain — \
			these are real chain replacements where the later collator's block was backed instead.\n",
			winner_replacement, pct(winner_replacement)
		);
	}

	// Slot boundary analysis
	if let Some(ref sba) = multi.slot_boundary_analysis {
		write_slot_boundary_section(out, sba, config);
	}

	// Sample rebuilds
	let sample_count = multi.rebuilds.len().min(config.max_samples);
	if sample_count > 0 {
		let _ = writeln!(out, "### Sample Rebuilds\n");
		for r in &multi.rebuilds[..sample_count] {
			let _ = writeln!(out, "#### Block #{} \u{2014} rebuilt on {}", r.block_number, r.observer);

			let slot_info = |slot: Option<u64>| {
				slot.map(|s| format!(" slot {}", s)).unwrap_or_default()
			};

			let _ = writeln!(
				out,
				"- Original best: {} (built by {} at {}{}{})",
				r.block_hash_best.short(),
				r.best_collator.as_deref().unwrap_or("unknown"),
				r.best_timestamp.format("%H:%M:%S%.3f"),
				r.best_relay_parent
					.map(|rp| format!(" on RP #{}", rp))
					.unwrap_or_default(),
				slot_info(r.best_slot),
			);
			let _ = writeln!(
				out,
				"- Replacement: {} (built by {} at {}{}{})",
				r.block_hash_rebuilt.short(),
				r.rebuilt_collator.as_deref().unwrap_or("unknown"),
				r.rebuilt_timestamp.format("%H:%M:%S%.3f"),
				r.rebuilt_relay_parent
					.map(|rp| format!(" on RP #{}", rp))
					.unwrap_or_default(),
				slot_info(r.rebuilt_slot),
			);

			// Root cause from classified cause
			match &r.cause {
				RebuildCause::SlotBoundaryOverlap { outgoing_slot, incoming_slot } => {
					let _ = writeln!(
						out,
						"- Root cause: slot boundary overlap (slot {} → {})",
						outgoing_slot, incoming_slot
					);
				},
				RebuildCause::NewerRelayParent { old_rp, new_rp } => {
					let _ = writeln!(
						out,
						"- Root cause: rebuilt on newer relay parent (RP #{} → #{})",
						old_rp, new_rp
					);
				},
				RebuildCause::Unknown => {
					let _ = writeln!(out, "- Root cause: unknown");
				},
			}
			match &r.on_chain_winner {
				OnChainWinner::OriginalBest => {
					let _ = writeln!(out, "- On-chain: **original best** was included");
				},
				OnChainWinner::Replacement => {
					let _ = writeln!(out, "- On-chain: **replacement** was included");
				},
				OnChainWinner::Unknown => {
					let _ = writeln!(out, "- On-chain: unknown (outside query range)");
				},
			}

			// Relay chain block sequence between best and rebuilt relay parents
			if !r.relay_block_sequence.is_empty() {
				let _ = writeln!(out, "- Relay chain block sequence:");
				for info in &r.relay_block_sequence {
					let hash_hex = hex::encode(&info.block_hash);
					let hash_short = format!("0x{}…{}", &hash_hex[..4], &hash_hex[hash_hex.len()-4..]);
					let pjs_link = format!(
						"https://polkadot.js.org/apps/?rpc=wss%3A%2F%2Frpc.polkadot.io#/explorer/query/0x{}",
						hash_hex
					);

					let n_backed = info.backed_para_candidates.len();
					let n_included = info.included_para_candidates.len();

					// Decode candidates into (block_number, hash_short) pairs
					let decode_candidates = |candidates: &[ParaCandidateEvent]| -> Vec<(u32, String)> {
						candidates.iter().filter_map(|c| {
							let hd = c.para_head_data.as_ref()?;
							let n = crate::chain_client::decode_para_block_number(hd)?;
							let block_hash = crate::chain_client::compute_block_hash_from_head_data(hd);
							let bh_short = format!("0x{}…{}", &hex::encode(&block_hash[..2]), &hex::encode(&block_hash[30..32]));
							Some((n, bh_short))
						}).collect()
					};

					let backed_decoded = decode_candidates(&info.backed_para_candidates);
					let included_decoded = decode_candidates(&info.included_para_candidates);

					// Format range summary: "10 (2758349-2758358)"
					let range_summary = |decoded: &[(u32, String)], count: usize| -> String {
						if count == 0 {
							return String::new();
						}
						let nums: Vec<u32> = decoded.iter().map(|(n, _)| *n).collect();
						if let (Some(&min), Some(&max)) = (nums.iter().min(), nums.iter().max()) {
							if min == max {
								format!("{} (#{})", count, min)
							} else {
								format!("{} ({}–{})", count, min, max)
							}
						} else {
							format!("{}", count)
						}
					};

					// Summary line
					let backed_summary = if n_backed == 0 {
						"no backed candidates for our para".to_string()
					} else {
						let mut groups: Vec<u32> = info.backed_para_candidates.iter()
							.filter_map(|c| c.group_index)
							.collect();
						groups.sort();
						groups.dedup();
						let group_note = if groups.is_empty() {
							String::new()
						} else {
							let g: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
							format!(" [group(s) {}]", g.join(", "))
						};
						format!("backs {}{}", range_summary(&backed_decoded, n_backed), group_note)
					};

					let included_summary = if n_included == 0 {
						String::new()
					} else {
						format!(", includes {}", range_summary(&included_decoded, n_included))
					};

					// ClaimQueue scheduling status
					let schedule_summary = if info.claim_queue_cores.is_empty() {
						" — **not scheduled**".to_string()
					} else {
						let cores: Vec<String> = info.claim_queue_cores.iter().map(|c| c.to_string()).collect();
						format!(" — scheduled on core(s) {}", cores.join(", "))
					};

					let _ = writeln!(
						out,
						"  - [#{}]({}) ({}): {}{}{}",
						info.block_number,
						pjs_link,
						hash_short,
						backed_summary,
						included_summary,
						schedule_summary,
					);

					// Detail lines: comma-separated on a single line per category
					if !backed_decoded.is_empty() {
						let items: Vec<String> = backed_decoded.iter()
							.map(|(n, bh)| format!("#{} {}", n, bh))
							.collect();
						let _ = writeln!(out, "    - backed {}", items.join(", "));
					}
					if !included_decoded.is_empty() {
						let items: Vec<String> = included_decoded.iter()
							.map(|(n, bh)| format!("#{} {}", n, bh))
							.collect();
						let _ = writeln!(out, "    - included {}", items.join(", "));
					}
				}

				// Scheduling summary for the viability window
				let total_rc_blocks = r.relay_block_sequence.len();
				let scheduled_count = r.relay_block_sequence.iter()
					.filter(|info| !info.claim_queue_cores.is_empty())
					.count();
				let unscheduled_count = total_rc_blocks - scheduled_count;
				if total_rc_blocks > 0 && unscheduled_count > 0 {
					let _ = writeln!(
						out,
						"- Scheduling: para was scheduled in {}/{} relay blocks in this range ({} unscheduled)",
						scheduled_count, total_rc_blocks, unscheduled_count,
					);
				}
			}
			let _ = writeln!(out);
		}
	}
}

fn write_duplicate_block_section(
	out: &mut String,
	multi: &MultiCollatorAnalysis,
) {
	let _ = writeln!(out, "## Duplicate Block Production Check\n");
	let _ = writeln!(
		out,
		"Checks whether 2+ different collators ever built the same para block number \
		on the same relay parent. This would indicate a protocol-level issue \
		(e.g. Aura slot assignment violation or shared collator keys).\n"
	);

	let dupes = &multi.duplicate_blocks_same_rp;

	if dupes.is_empty() {
		let _ = writeln!(
			out,
			"**No duplicates found.** Every (block number, relay parent) pair was produced \
			by exactly one collator. Slot assignment is clean.\n"
		);
		return;
	}

	let _ = writeln!(
		out,
		"**WARNING: {} case(s)** where different collators built the same block on the same RP:\n",
		dupes.len()
	);

	let _ = writeln!(out, "| Block # | Relay Parent | Collators | Slots | Time Gap |");
	let _ = writeln!(out, "|---|---|---|---|---|");

	for dupe in dupes {
		let mut producers = dupe.producers.clone();
		producers.sort_by_key(|p| p.timestamp);

		let collators: Vec<&str> = producers.iter().map(|p| p.collator.as_str()).collect();
		let slots: Vec<String> = producers.iter().map(|p| format!("{}", p.slot)).collect();

		let time_gap = if producers.len() >= 2 {
			let first = producers.first().unwrap().timestamp;
			let last = producers.last().unwrap().timestamp;
			let gap_ms = (last - first).num_milliseconds();
			format!("{}ms", gap_ms)
		} else {
			"-".to_string()
		};

		let _ = writeln!(
			out,
			"| {} | #{} | {} | {} | {} |",
			dupe.block_number,
			dupe.relay_parent_num,
			collators.join(", "),
			slots.join(", "),
			time_gap,
		);
	}
	let _ = writeln!(out);

	// Detailed samples (up to 5)
	let sample_count = dupes.len().min(5);
	if sample_count > 0 {
		let _ = writeln!(out, "### Duplicate Production Details\n");
		for dupe in &dupes[..sample_count] {
			let _ = writeln!(
				out,
				"#### Block #{} on RP #{}\n",
				dupe.block_number, dupe.relay_parent_num
			);
			let mut producers = dupe.producers.clone();
			producers.sort_by_key(|p| p.timestamp);
			for p in &producers {
				let _ = writeln!(
					out,
					"- **{}**: slot {} at {} — hash {}",
					p.collator,
					p.slot,
					p.timestamp.format("%H:%M:%S%.3f"),
					p.block_hash.short(),
				);
			}
			let _ = writeln!(out);
		}
	}
}

fn write_slot_boundary_section(
	out: &mut String,
	sba: &SlotBoundaryAnalysis,
	config: &ReportConfig,
) {
	if sba.boundaries.is_empty() {
		return;
	}

	let _ = writeln!(out, "### Slot Boundary Propagation Analysis\n");
	let _ = writeln!(
		out,
		"Measures the time gap between the outgoing collator sealing its last block \
		and the incoming collator starting to build. Negative = overlap (incoming started before \
		outgoing's block was propagated).\n"
	);

	let _ = writeln!(out, "| Metric | Value |");
	let _ = writeln!(out, "|---|---|");
	let _ = writeln!(out, "| Total slot boundaries | {} |", sba.boundaries.len());
	let _ = writeln!(
		out,
		"| Boundaries with overlap | **{}** ({:.0}%) |",
		sba.boundaries_with_overlap,
		sba.boundaries_with_overlap as f64 / sba.boundaries.len() as f64 * 100.0
	);
	let _ = writeln!(
		out,
		"| Boundaries without overlap | {} ({:.0}%) |",
		sba.boundaries_without_overlap,
		sba.boundaries_without_overlap as f64 / sba.boundaries.len() as f64 * 100.0
	);
	let _ = writeln!(
		out,
		"| Propagation gap (avg) | **{:.0}ms** |",
		sba.avg_propagation_gap_ms
	);
	let _ = writeln!(
		out,
		"| Propagation gap (median) | **{}ms** |",
		sba.median_propagation_gap_ms
	);
	let _ = writeln!(
		out,
		"| Propagation gap (min / max) | {}ms / {}ms |",
		sba.min_propagation_gap_ms, sba.max_propagation_gap_ms
	);
	if sba.boundaries_with_overlap > 0 {
		let _ = writeln!(
			out,
			"| Avg overlap per boundary (when overlapping) | **{:.1} blocks** |",
			sba.avg_overlap_count
		);
		let _ = writeln!(
			out,
			"| Max overlap at a single boundary | **{} blocks** |",
			sba.max_overlap_count
		);
	}
	if sba.relay_parent_gaps > 0 {
		let _ = writeln!(
			out,
			"| Relay parent gaps > 1 block | **{}** ({:.0}%) — possible relay chain forks |",
			sba.relay_parent_gaps,
			sba.relay_parent_gaps as f64 / sba.boundaries.len() as f64 * 100.0
		);
	}
	let _ = writeln!(out);

	// Show worst-overlap boundaries as samples
	let mut worst: Vec<&SlotBoundaryDetail> = sba.boundaries.iter().collect();
	worst.sort_by(|a, b| {
		b.overlap_count
			.cmp(&a.overlap_count)
			.then(a.propagation_gap_ms.cmp(&b.propagation_gap_ms))
	});

	let sample_count = worst.len().min(config.max_samples);
	let worst_with_overlap: Vec<&&SlotBoundaryDetail> =
		worst.iter().filter(|b| b.overlap_count > 0).take(sample_count).collect();

	if !worst_with_overlap.is_empty() {
		let _ = writeln!(out, "#### Worst overlap boundaries\n");
		for b in &worst_with_overlap {
			let _ = writeln!(
				out,
				"- Slot {} ({}) → {} ({}): gap **{}ms**, overlap **{} blocks**, RP #{} → #{}",
				b.outgoing_slot,
				b.outgoing_collator,
				b.incoming_slot,
				b.incoming_collator,
				b.propagation_gap_ms,
				b.overlap_count,
				b.outgoing_rp,
				b.incoming_rp,
			);
		}
		let _ = writeln!(out);
	}
}

fn format_duration(duration: chrono::Duration) -> String {
	let total_secs = duration.num_seconds();
	if total_secs < 3600 {
		format!("{} minute", total_secs / 60)
	} else if total_secs < 86400 {
		let hours = total_secs / 3600;
		let mins = (total_secs % 3600) / 60;
		if mins > 0 {
			format!("{} hour {} minute", hours, mins)
		} else {
			format!("{} hour", hours)
		}
	} else {
		let days = total_secs / 86400;
		let hours = (total_secs % 86400) / 3600;
		if hours > 0 {
			format!("{} day {} hour", days, hours)
		} else {
			format!("{} day", days)
		}
	}
}
