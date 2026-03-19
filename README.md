# Block Confidence Monitor

A Rust CLI tool that analyzes Polkadot parachain collator logs to detect and classify dropped blocks. It cross-references local log data with on-chain relay chain state to produce detailed reports on block production reliability, collator performance, and failure root causes.

## Why

Parachain collators build blocks that must be backed and included by relay chain validators.
When a locally built block never appears on-chain, the collator has experienced a "drop."
Understanding *why* blocks are dropped — session boundary timing, relay parent expiration, fork mismatches, or collator competition — is critical for infrastructure operators tuning their setups.

This tool automates that investigation by parsing collator logs, querying relay chain RPCs, and classifying every drop into an actionable category.

## Features

- **Drop detection & classification** — identifies blocks that were built locally but never included on-chain, and classifies each by root cause:
  - **Session boundary** — block lost during a validator session transition
  - **Relay parent expired** — relay parent fell outside the viability window before backing
  - **Wrong fork** — relay parent was on a pruned/reorganized chain
  - **Unknown** — insufficient data to determine cause
- **Block confidence metric** — percentage of locally authored blocks that were adopted as the canonical ("best") block
- **Multi-collator analysis** — when multiple collator logs are provided, detects rebuild events (one collator's block replacing another's) and computes per-collator win/loss rates
- **Rebuild classification** — categorizes rebuilds as slot boundary overlap, newer relay parent, or unknown
- **Concurrent RPC queries** — semaphore-controlled parallel queries to the relay chain for fast data collection
- **Session boundary detection** — binary search over relay blocks to efficiently locate session transitions
- **Truncated hash resolution** — matches truncated log hashes (`0xabcd…ef01`) to full 32-byte hashes via a registry
- **Markdown reports** — structured output with tables, sample data points, and relay block sequences

## Usage

### Single collator

Analyze logs from a single collator node:

```bash
block-confidence-monitor \
  collator.log \
  --rpc-url wss://kusama-rpc.polkadot.io \
  --para-id 2000 \
  -o report.md
```

### Multiple collators

Analyze logs from several collators simultaneously. Place one `.txt` file per collator in a directory — the filename (without extension) is used as the collator identifier:

```bash
block-confidence-monitor \
  --log-dir ./collator-logs/ \
  --rpc-url wss://kusama-rpc.polkadot.io \
  --para-id 2000 \
  -o report.md
```

## How it works

The tool runs a 7-phase pipeline:

1. **Parse** — regex-based extraction of events from collator logs (block imports, build attempts, pre-sealed blocks, collation submissions, fetch latencies, view updates). A hash registry maps truncated hashes to their full forms.

2. **Identify built blocks** — correlates `Pre-sealed` events with `BuildAttempt` and `CollationSubmission` events by timing (within 2s), hash, and block number to reconstruct which blocks the collator actually produced.

3. **Connect to relay chain** — establishes a `subxt` RPC connection with configurable concurrency limits.

4. **Find session boundaries** — binary search across the relay block range to locate session index transitions.

5. **Detect & classify drops** — queries on-chain `ParaInclusion::CandidateBacked` and `CandidateIncluded` events, identifies built blocks not in the included set, and classifies each drop by cause.

6. **Multi-collator analysis** *(optional)* — cross-references `Pre-sealed` events across collators to detect rebuild events and attribute authorship wins/losses.

7. **Generate report** — produces a Markdown report with block confidence metrics, drop summaries, session boundary statistics, and sample data points.

## Report output

The generated Markdown report includes:

- **Block Confidence** — table showing blocks built (best), blocks rebuilt (non-best), total imports, confidence percentage, and rebuild rate
- **Summary** — time window, total blocks built/included/dropped, and relay block range
- **Session boundary drops** — count and details of blocks lost at session transitions
- **Non-session drops** — breakdown by relay parent expired, wrong fork, and unknown causes
- **Sample data points** — per-category examples showing relay block sequences, timestamps, and context
- **Multi-collator section** *(when applicable)* — per-collator authorship table (built/won/lost/win rate) and rebuild event breakdown

### Downloading logs

```bash
tsh login --proxy=teleport.parity.io:443
tsh proxy app loki --port 10700

logcli query --addr=http://localhost:10700 --timezone=UTC --from="2026-03-17T11:00:34Z" --to="2026-03-17T11:56:34Z" '{chain="yap-polkadot-3428", node="polkadot-yap-3428-node-0"} ' --batch 5000 --limit 100000  --org-id='parity-hosted-mainnet' > polkadot-yap-3428-node-0-long.txt
```

### Kusama YAP

```bash
block-confidence-monitor \
    --log-dir ../yap-3392-kusama-logs/ \
    --rpc-url wss://rpc-kusama.helixstreet.io  \
    --para-id 3392 \
    --chain-name Kusama
```

### Polkadot YAP

```bash
block-confidence-monitor \
    --log-dir ../yap-3428-polkadot-logs/ \
    --rpc-url wss://rpc-polkadot.helixstreet.io  \
    --para-id 3428 \
    --chain-name Polkadot
```
