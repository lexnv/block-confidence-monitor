#[allow(dead_code)]
mod analyzer;
mod chain_client;
mod log_parser;
mod report;
#[allow(dead_code)]
mod types;

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "block-confidence-monitor")]
#[command(about = "Analyzes parachain collator logs to detect and classify dropped blocks, supplementing with on-chain relay chain data.")]
struct Cli {
	/// Path to the collator log file (mutually exclusive with --log-dir)
	log_file: Option<PathBuf>,

	/// Directory containing collator log files (*.txt), one per collator.
	/// The filename (without extension) is used as the collator ID.
	#[arg(long)]
	log_dir: Option<PathBuf>,

	/// RPC URL for the relay chain (e.g. wss://kusama-rpc.polkadot.io)
	#[arg(long)]
	rpc_url: String,

	/// Parachain ID to analyze
	#[arg(long)]
	para_id: u32,

	/// Relay parent viability window in blocks (default: 3)
	#[arg(long, default_value = "3")]
	viability_window: u32,

	/// Maximum number of sample data points per category in the report (default: 5)
	#[arg(long, default_value = "5")]
	max_samples: usize,

	/// Maximum concurrent RPC queries (default: 10)
	#[arg(long, default_value = "10")]
	concurrency: usize,

	/// Output file path (default: stdout)
	#[arg(long, short)]
	output: Option<PathBuf>,

	/// Chain name for the report header (default: "Relay Chain")
	#[arg(long, default_value = "Relay Chain")]
	chain_name: String,

	/// Show detailed incident chain visualization at the end of the report
	#[arg(long)]
	detailed: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| "block_confidence_monitor=info".into()),
		)
		.with_target(false)
		.init();

	let cli = Cli::parse();

	// Validate: exactly one of log_file or log_dir must be set
	match (&cli.log_file, &cli.log_dir) {
		(None, None) => bail!("Either a log file path or --log-dir must be provided"),
		(Some(_), Some(_)) => bail!("Cannot specify both a log file and --log-dir"),
		_ => {},
	}

	if let Some(ref log_dir) = cli.log_dir {
		run_multi_collator(&cli, log_dir).await
	} else {
		let log_file = cli.log_file.as_ref().expect("validated above");
		run_single_collator(&cli, log_file).await
	}
}

async fn run_single_collator(cli: &Cli, log_file: &PathBuf) -> Result<()> {
	// Phase 1: Parse log file
	tracing::info!(path = %log_file.display(), "Parsing log file...");
	let parsed = log_parser::parse_log(log_file, cli.para_id)?;

	let (min_relay, max_relay) = match (parsed.min_relay_block(), parsed.max_relay_block()) {
		(Some(min), Some(max)) => (min, max),
		_ => {
			tracing::error!("No relay chain block imports found in log file");
			return Ok(());
		},
	};

	if let Some((start, end)) = parsed.time_range() {
		let duration = end - start;
		tracing::info!(
			start = %start.format("%Y-%m-%d %H:%M:%S"),
			end = %end.format("%Y-%m-%d %H:%M:%S"),
			duration_secs = duration.num_seconds(),
			relay_blocks = format!("{}..{}", min_relay, max_relay),
			"Log time range"
		);
	}

	// Phase 2: Identify locally built blocks
	let built_blocks = analyzer::identify_built_blocks(&parsed);
	if built_blocks.is_empty() {
		tracing::warn!("No locally built blocks detected (no Pre-sealed events found). \
			Make sure the log includes aura::cumulus logs at INFO level.");
	}

	// Phase 3: Connect to relay chain
	let client =
		chain_client::ChainClient::new(&cli.rpc_url, cli.para_id, cli.concurrency).await?;

	// Phase 4: Find session boundaries
	tracing::info!("Searching for session boundaries...");
	let session_boundaries = client.find_session_boundaries(min_relay, max_relay).await?;

	// Phase 5: Detect and classify drops
	tracing::info!("Detecting dropped blocks and querying on-chain data...");
	let analysis = analyzer::detect_and_classify_drops(
		&built_blocks,
		&parsed,
		&client,
		&session_boundaries,
		cli.viability_window,
	)
	.await?;

	tracing::info!(
		session_boundary = analysis.session_boundary_drops.len(),
		relay_parent_expired = analysis.relay_parent_expired_drops.len(),
		wrong_fork = analysis.wrong_fork_drops.len(),
		unknown = analysis.unknown_drops.len(),
		"Classification complete"
	);

	// Phase 6: Generate report
	let report_config = report::ReportConfig {
		chain_name: cli.chain_name.clone(),
		max_samples: cli.max_samples,
		viability_window: cli.viability_window,
		para_id: cli.para_id,
		rpc_url: cli.rpc_url.clone(),
		n_collators: 1,
		detailed: cli.detailed,
	};

	let report_text = report::generate_report(&analysis, &report_config, &parsed);

	// Phase 7: Output
	output_report(&cli.output, &report_text)
}

async fn run_multi_collator(cli: &Cli, log_dir: &PathBuf) -> Result<()> {
	// Phase 1: Parse all collator log files
	let multi = log_parser::parse_log_dir(log_dir, cli.para_id)?;

	let (min_relay, max_relay) = match multi.relay_block_range() {
		Some((min, max)) => (min, max),
		None => {
			tracing::error!("No relay chain block imports found in any log file");
			return Ok(());
		},
	};

	if let Some((start, end)) = multi.time_range() {
		let duration = end - start;
		tracing::info!(
			start = %start.format("%Y-%m-%d %H:%M:%S"),
			end = %end.format("%Y-%m-%d %H:%M:%S"),
			duration_secs = duration.num_seconds(),
			relay_blocks = format!("{}..{}", min_relay, max_relay),
			collators = multi.collators.len(),
			"Multi-collator log time range"
		);
	}

	// Use the first collator as the "primary" for drop analysis
	let primary_name = multi
		.collators
		.keys()
		.next()
		.expect("at least one collator; qed")
		.clone();
	let primary_log = &multi.collators[&primary_name];

	// Phase 2: Identify locally built blocks from the primary collator
	let built_blocks = analyzer::identify_built_blocks(primary_log);
	if built_blocks.is_empty() {
		tracing::warn!(
			collator = %primary_name,
			"No locally built blocks detected on primary collator"
		);
	}

	// Phase 3: Connect to relay chain
	let client =
		chain_client::ChainClient::new(&cli.rpc_url, cli.para_id, cli.concurrency).await?;

	// Phase 4: Find session boundaries
	tracing::info!("Searching for session boundaries...");
	let session_boundaries = client.find_session_boundaries(min_relay, max_relay).await?;

	// Phase 5: Detect and classify drops (using primary collator)
	tracing::info!("Detecting dropped blocks and querying on-chain data...");
	let analysis = analyzer::detect_and_classify_drops(
		&built_blocks,
		primary_log,
		&client,
		&session_boundaries,
		cli.viability_window,
	)
	.await?;

	tracing::info!(
		session_boundary = analysis.session_boundary_drops.len(),
		relay_parent_expired = analysis.relay_parent_expired_drops.len(),
		wrong_fork = analysis.wrong_fork_drops.len(),
		unknown = analysis.unknown_drops.len(),
		"Classification complete"
	);

	// Phase 5b: Multi-collator rebuild analysis
	tracing::info!("Analyzing rebuilds across collators...");

	// Build on-chain data map for rebuild analysis
	// Reuse the relay block range from the analysis
	let mut blocks_to_query = std::collections::BTreeSet::new();
	for bb in &built_blocks {
		let rp = bb.relay_parent_num;
		for offset in 0..=(cli.viability_window + 2) {
			blocks_to_query.insert(rp + offset);
		}
		if rp > 0 {
			blocks_to_query.insert(rp - 1);
		}
	}
	let blocks_vec: Vec<u32> = blocks_to_query.into_iter().collect();
	let on_chain = if !blocks_vec.is_empty() {
		client.query_relay_block_range(&blocks_vec).await?
	} else {
		std::collections::BTreeMap::new()
	};

	let multi_analysis = analyzer::analyze_rebuilds(&multi, &on_chain);

	tracing::info!(
		rebuild_events = multi_analysis.rebuilds.len(),
		collators = multi_analysis.per_collator.len(),
		"Multi-collator analysis complete"
	);

	// Phase 6: Generate report
	let report_config = report::ReportConfig {
		chain_name: cli.chain_name.clone(),
		max_samples: cli.max_samples,
		viability_window: cli.viability_window,
		para_id: cli.para_id,
		rpc_url: cli.rpc_url.clone(),
		n_collators: multi.collators.len(),
		detailed: cli.detailed,
	};

	let report_text = report::generate_multi_collator_report(
		&analysis,
		&report_config,
		&multi_analysis,
		primary_log,
	);

	// Phase 7: Output
	output_report(&cli.output, &report_text)
}

fn output_report(output: &Option<PathBuf>, report_text: &str) -> Result<()> {
	match output {
		Some(path) => {
			std::fs::write(path, report_text)?;
			tracing::info!(path = %path.display(), "Report written to file");
		},
		None => {
			println!("{}", report_text);
		},
	}
	Ok(())
}
