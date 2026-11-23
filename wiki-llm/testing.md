# Testing
- Protocol fixtures: `python3 -I crates/claude/scripts/record.py <scenario> <sandbox-dir>`; scenarios are listed in the script. Record in `$(mktemp -d)`: a cwd under the home or scratchpad path leaks the user name through streamed chunks, and the script refuses such fixtures. Output is redacted (home path, user name, emails, account keys).
- Manual acceptance checklist: added in P6.
