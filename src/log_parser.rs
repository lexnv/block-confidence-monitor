use crate::types::*;
use anyhow::{Context, Result};
use chrono::{NaiveDateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::OnceLock;

struct Patterns {
	timestamp: Regex,
	block_import: Regex,
	not_building: Regex,
	building: Regex,
	pre_sealed: Regex,
	collation_submit: Regex,
	fetch_latency: Regex,
	candidate_generated: Regex,
	view_update: Regex,
	collation_expired: Regex,
}

fn patterns() -> &'static Patterns {
	static INSTANCE: OnceLock<Patterns> = OnceLock::new();
	INSTANCE.get_or_init(|| Patterns {
		// Millisecond timestamp from the inner log message
		timestamp: Regex::new(r"(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})").unwrap(),

		// [Parachain] 🏆 Imported #1713769 (0xe05d…b05e → 0xd58a…29a8)
		block_import: Regex::new(
			r"\[(Parachain|Relaychain)\]\s+[^\s]+\s+Imported\s+#(\d+)\s+\((0x[0-9a-f\u{2026}]+)\s+→\s+(0x[0-9a-f\u{2026}]+)\)"
		).unwrap(),

		// Not building block. unincluded_segment_len=33 relay_parent=0x... relay_parent_num=30400619 included_hash=0x... included_num=1713735 parent=0x... slot=Slot(295627304)
		not_building: Regex::new(
			r"Not building block\.\s+unincluded_segment_len=(\d+)\s+relay_parent=(0x[0-9a-f]+)\s+relay_parent_num=(\d+)\s+included_hash=(0x[0-9a-f]+)\s+included_num=(\d+)\s+parent=(0x[0-9a-f]+)\s+slot=Slot\((\d+)\)"
		).unwrap(),

		// Building block. unincluded_segment_len=38 relay_parent=0x5ec7…83e9 relay_parent_num=30400616 relay_parent_offset=1 included_hash=0xdd3c…adde included_num=1713705 parent=0x608f…580d slot=Slot(295627301)
		building: Regex::new(
			r"Building block\.\s+unincluded_segment_len=(\d+)\s+relay_parent=(0x[0-9a-f\u{2026}]+)\s+relay_parent_num=(\d+)\s+(?:relay_parent_offset=(\d+)\s+)?included_hash=(0x[0-9a-f\u{2026}]+)\s+included_num=(\d+)\s+parent=(0x[0-9a-f\u{2026}]+)\s+slot=Slot\((\d+)\)"
		).unwrap(),

		// Pre-sealed block for proposal at 1713743. Hash now 0x608f..., previously 0x5b58...
		pre_sealed: Regex::new(
			r"Pre-sealed block for proposal at (\d+)\.\s+Hash now (0x[0-9a-f]+),\s+previously (0x[0-9a-f]+)"
		).unwrap(),

		// Submitting collation for core. core_index=CoreIndex(62) hash=0x608f... number=1713743
		collation_submit: Regex::new(
			r"Submitting collation for core\.\s+core_index=CoreIndex\((\d+)\)\s+hash=(0x[0-9a-f\u{2026}]+)\s+number=(\d+)"
		).unwrap(),

		// Collation fetch latency is 3ms para_head=0x608f... our_para_id=3428
		fetch_latency: Regex::new(
			r"Collation fetch latency is (\d+)ms\s+para_head=(0x[0-9a-f]+)\s+our_para_id=(\d+)"
		).unwrap(),

		// Candidate generated candidate_hash=0x... pov_hash=0x... relay_parent=0x... para_id=3428 core_index=CoreIndex(62)
		candidate_generated: Regex::new(
			r"Candidate generated\s+candidate_hash=(0x[0-9a-f]+)\s+pov_hash=0x[0-9a-f]+\s+relay_parent=(0x[0-9a-f]+)\s+para_id=(\d+)\s+core_index=CoreIndex\((\d+)\)"
		).unwrap(),

		// Our view updated, current view: OurView { view: View { heads: [...], finalized_number: 30400616 } }
		view_update: Regex::new(
			r"Our view updated.*heads:\s*\[([^\]]*)\],\s*finalized_number:\s*(\d+)"
		).unwrap(),

		// Collation expired age=4 collation_state="advertised" relay_parent=(0xd14e...bf1b, 30511804) para_id=Id(3428) head=0x... candidate_hash=0x...
		// Fields may have quotes or not depending on tracing subscriber
		collation_expired: Regex::new(
			r#"Collation expired\s+age=(\d+)\s+collation_state="?(\w+)"?\s+relay_parent=\(?(0x[0-9a-f]+),?\s*(\d+)\)?"#
		).unwrap(),
	})
}

fn parse_timestamp(line: &str) -> Option<chrono::DateTime<Utc>> {
	let pat = patterns();
	let caps = pat.timestamp.captures(line)?;
	let ts_str = caps.get(1)?.as_str();
	let ndt = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S%.3f").ok()?;
	Some(ndt.and_utc())
}

pub fn parse_log(path: &Path, para_id: u32) -> Result<ParsedLog> {
	let file = File::open(path).context("Failed to open log file")?;
	let file_size = file.metadata()?.len();
	let reader = BufReader::with_capacity(128 * 1024, file);

	let pb = ProgressBar::new(file_size);
	pb.set_style(
		ProgressStyle::default_bar()
			.template("[{elapsed_precise}] {bar:40} {bytes}/{total_bytes} ({eta})")
			.unwrap(),
	);

	let mut log = ParsedLog::default();
	let pat = patterns();
	let mut line = String::with_capacity(512);
	let mut lines_read: u64 = 0;
	let mut reader = reader;

	loop {
		line.clear();
		let bytes_read = reader.read_line(&mut line)?;
		if bytes_read == 0 {
			break;
		}

		lines_read += 1;
		if lines_read % 10_000 == 0 {
			pb.set_position(pb.position() + (bytes_read as u64) * 10_000);
		}

		// Pre-filter: check for any known pattern substring before regex
		let dominated = if line.contains("Imported #") {
			1
		} else if line.contains("Not building block") {
			2
		} else if line.contains("Building block.") && !line.contains("Not building") {
			3
		} else if line.contains("Pre-sealed block") {
			4
		} else if line.contains("Submitting collation") {
			5
		} else if line.contains("Collation fetch latency") {
			6
		} else if line.contains("Candidate generated") {
			7
		} else if line.contains("Our view updated") {
			8
		} else if line.contains("Collation expired") {
			9
		} else {
			0
		};

		if dominated == 0 {
			continue;
		}

		let ts = match parse_timestamp(&line) {
			Some(ts) => ts,
			None => continue,
		};

		match dominated {
			1 => {
				// Block import (para or relay)
				if let Some(caps) = pat.block_import.captures(&line) {
					let chain = caps.get(1).unwrap().as_str();
					let number: u32 = caps.get(2).unwrap().as_str().parse().unwrap_or(0);
					let parent = caps.get(3).unwrap().as_str();
					let hash = caps.get(4).unwrap().as_str();

					if chain == "Parachain" {
						let is_best = line.contains('🏆');
						log.para_imports.push(ParaBlockImport {
							block_number: number,
							parent_hash: LogHash::from_auto(parent),
							block_hash: LogHash::from_auto(hash),
							timestamp: ts,
							is_best,
							collator: None,
						});
					} else {
						log.relay_imports.push(RelayBlockImport {
							block_number: number,
							parent_hash: LogHash::from_auto(parent),
							block_hash: LogHash::from_auto(hash),
							timestamp: ts,
						});
						// Register relay block hashes
						if !hash.contains('…') {
							log.hash_registry.register_full(hash);
						}
						if !parent.contains('…') {
							log.hash_registry.register_full(parent);
						}
					}
				}
			},
			2 => {
				// Not building block (has FULL hashes)
				if let Some(caps) = pat.not_building.captures(&line) {
					let rp = caps.get(2).unwrap().as_str();
					let ih = caps.get(4).unwrap().as_str();
					let parent = caps.get(6).unwrap().as_str();

					log.hash_registry.register_full(rp);
					log.hash_registry.register_full(ih);
					log.hash_registry.register_full(parent);

					log.build_attempts.push(BuildAttempt {
						building: false,
						unincluded_segment_len: caps
							.get(1)
							.unwrap()
							.as_str()
							.parse()
							.unwrap_or(0),
						relay_parent: LogHash::from_full(rp),
						relay_parent_num: caps.get(3).unwrap().as_str().parse().unwrap_or(0),
						relay_parent_offset: None,
						included_hash: LogHash::from_full(ih),
						included_num: caps.get(5).unwrap().as_str().parse().unwrap_or(0),
						parent: LogHash::from_full(parent),
						slot: caps.get(7).unwrap().as_str().parse().unwrap_or(0),
						timestamp: ts,
					});
				}
			},
			3 => {
				// Building block (truncated hashes, has relay_parent_offset)
				if let Some(caps) = pat.building.captures(&line) {
					let rp = caps.get(2).unwrap().as_str();
					let ih = caps.get(5).unwrap().as_str();
					let parent = caps.get(7).unwrap().as_str();

					if !rp.contains('…') {
						log.hash_registry.register_full(rp);
					}
					if !ih.contains('…') {
						log.hash_registry.register_full(ih);
					}
					if !parent.contains('…') {
						log.hash_registry.register_full(parent);
					}

					log.build_attempts.push(BuildAttempt {
						building: true,
						unincluded_segment_len: caps
							.get(1)
							.unwrap()
							.as_str()
							.parse()
							.unwrap_or(0),
						relay_parent: LogHash::from_auto(rp),
						relay_parent_num: caps.get(3).unwrap().as_str().parse().unwrap_or(0),
						relay_parent_offset: caps.get(4).map(|m| m.as_str().parse().unwrap_or(0)),
						included_hash: LogHash::from_auto(ih),
						included_num: caps.get(6).unwrap().as_str().parse().unwrap_or(0),
						parent: LogHash::from_auto(parent),
						slot: caps.get(8).unwrap().as_str().parse().unwrap_or(0),
						timestamp: ts,
					});
				}
			},
			4 => {
				// Pre-sealed block (full hashes)
				if let Some(caps) = pat.pre_sealed.captures(&line) {
					let post = caps.get(2).unwrap().as_str();
					let pre = caps.get(3).unwrap().as_str();
					log.hash_registry.register_full(post);
					log.hash_registry.register_full(pre);

					log.pre_sealed.push(PreSealedBlock {
						block_number: caps.get(1).unwrap().as_str().parse().unwrap_or(0),
						post_hash: LogHash::from_full(post),
						pre_hash: LogHash::from_full(pre),
						timestamp: ts,
					});
				}
			},
			5 => {
				// Collation submission
				if let Some(caps) = pat.collation_submit.captures(&line) {
					let hash = caps.get(2).unwrap().as_str();
					if !hash.contains('…') {
						log.hash_registry.register_full(hash);
					}
					log.collation_submissions.push(CollationSubmission {
						core_index: caps.get(1).unwrap().as_str().parse().unwrap_or(0),
						hash: LogHash::from_auto(hash),
						block_number: caps.get(3).unwrap().as_str().parse().unwrap_or(0),
						timestamp: ts,
					});
				}
			},
			6 => {
				// Collation fetch latency
				if let Some(caps) = pat.fetch_latency.captures(&line) {
					let pid: u32 = caps.get(3).unwrap().as_str().parse().unwrap_or(0);
					if pid == para_id {
						let head = caps.get(2).unwrap().as_str();
						log.hash_registry.register_full(head);
						log.collation_fetches.push(CollationFetchLatency {
							latency_ms: caps.get(1).unwrap().as_str().parse().unwrap_or(0),
							para_head: LogHash::from_full(head),
							para_id: pid,
							timestamp: ts,
						});
					}
				}
			},
			7 => {
				// Candidate generated
				if let Some(caps) = pat.candidate_generated.captures(&line) {
					let pid: u32 = caps.get(3).unwrap().as_str().parse().unwrap_or(0);
					if pid == para_id {
						let ch = caps.get(1).unwrap().as_str();
						let rp = caps.get(2).unwrap().as_str();
						log.hash_registry.register_full(ch);
						log.hash_registry.register_full(rp);
						log.candidates_generated.push(CandidateGenerated {
							candidate_hash: LogHash::from_full(ch),
							relay_parent: LogHash::from_full(rp),
							para_id: pid,
							core_index: caps.get(4).unwrap().as_str().parse().unwrap_or(0),
							timestamp: ts,
						});
					}
				}
			},
			8 => {
				// View update
				if let Some(caps) = pat.view_update.captures(&line) {
					let heads_str = caps.get(1).unwrap().as_str();
					let heads: Vec<String> = heads_str
						.split(',')
						.map(|s| s.trim().to_string())
						.filter(|s| !s.is_empty())
						.collect();
					for h in &heads {
						log.hash_registry.register_full(h);
					}
					log.view_updates.push(ViewUpdate {
						heads,
						finalized_number: caps.get(2).unwrap().as_str().parse().unwrap_or(0),
						timestamp: ts,
					});
				}
			},
			9 => {
				// Collation expired
				if let Some(caps) = pat.collation_expired.captures(&line) {
					let age: u32 = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
					let state = caps.get(2).unwrap().as_str().to_string();
					let rp_hash_str = caps.get(3).unwrap().as_str();
					let rp_num: u32 = caps.get(4).unwrap().as_str().parse().unwrap_or(0);

					if !rp_hash_str.contains('…') {
						log.hash_registry.register_full(rp_hash_str);
					}

					// Try to extract head and candidate_hash from the rest of the line
					let head = {
						static HEAD_RE: OnceLock<Regex> = OnceLock::new();
						let re = HEAD_RE.get_or_init(|| Regex::new(r"head=(0x[0-9a-f]+)").unwrap());
						re.captures(&line).map(|c| {
							let h = c.get(1).unwrap().as_str();
							if !h.contains('…') {
								log.hash_registry.register_full(h);
							}
							LogHash::from_auto(h)
						})
					};

					let candidate_hash = {
						static CH_RE: OnceLock<Regex> = OnceLock::new();
						let re = CH_RE.get_or_init(|| Regex::new(r"candidate_hash=(0x[0-9a-f]+)").unwrap());
						re.captures(&line).map(|c| {
							let h = c.get(1).unwrap().as_str();
							if !h.contains('…') {
								log.hash_registry.register_full(h);
							}
							LogHash::from_auto(h)
						})
					};

					log.collation_expired.push(CollationExpired {
						collation_state: state,
						relay_parent_num: rp_num,
						relay_parent_hash: LogHash::from_auto(rp_hash_str),
						age: Some(age),
						head,
						candidate_hash,
						timestamp: ts,
						produced_at: None,
						advertised_at: None,
						fetched_at: None,
					});
				}
			},
			_ => {},
		}
	}

	pb.finish_with_message("Log parsing complete");

	// The file is in reverse chronological order, so reverse all vectors
	log.para_imports.reverse();
	log.relay_imports.reverse();
	log.build_attempts.reverse();
	log.pre_sealed.reverse();
	log.collation_submissions.reverse();
	log.collation_fetches.reverse();
	log.candidates_generated.reverse();
	log.view_updates.reverse();
	log.collation_expired.reverse();

	tracing::info!(
		para_imports = log.para_imports.len(),
		relay_imports = log.relay_imports.len(),
		build_attempts = log.build_attempts.len(),
		pre_sealed = log.pre_sealed.len(),
		collation_submissions = log.collation_submissions.len(),
		collation_fetches = log.collation_fetches.len(),
		candidates_generated = log.candidates_generated.len(),
		collation_expired = log.collation_expired.len(),
		"Log parsing summary"
	);

	Ok(log)
}

/// Parse all `*.txt` files in a directory, one per collator.
/// The filename (without extension) is used as the collator name.
pub fn parse_log_dir(dir: &Path, para_id: u32) -> Result<MultiCollatorLogs> {
	let mut entries: Vec<_> = std::fs::read_dir(dir)
		.with_context(|| format!("Failed to read log directory: {}", dir.display()))?
		.filter_map(|e| e.ok())
		.filter(|e| {
			e.path().extension().map_or(false, |ext| ext == "txt")
		})
		.collect();

	entries.sort_by_key(|e| e.file_name());

	if entries.is_empty() {
		anyhow::bail!("No *.txt files found in {}", dir.display());
	}

	tracing::info!(
		dir = %dir.display(),
		count = entries.len(),
		"Found collator log files"
	);

	let mut collators = BTreeMap::new();
	for entry in &entries {
		let path = entry.path();
		let name = path.file_stem()
			.and_then(|s| s.to_str())
			.unwrap_or("unknown")
			.to_string();

		tracing::info!(collator = %name, path = %path.display(), "Parsing collator log...");
		let mut log = parse_log(&path, para_id)?;

		// Tag all para imports with the collator name
		for import in &mut log.para_imports {
			import.collator = Some(name.clone());
		}

		collators.insert(name, log);
	}

	Ok(MultiCollatorLogs { collators })
}
