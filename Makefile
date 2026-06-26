# mission-control — convenience targets.
# `mc` is the installed binary (cargo install / ~/.cargo/bin/mc).

.PHONY: summarize

# One-shot cross-workspace daily summary. Writes
#   ~/agents/obsAgents/mc-workspaces-summaries/YYYY-MM-DD-HH-summary.md
# (override the vault root with $OBS_AGENTS). Uses the installed `mc` on PATH.
summarize:
	mc summarize
