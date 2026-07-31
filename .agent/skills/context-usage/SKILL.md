---
name: context-usage
description: Read Codex or Claude session context usage from local transcript files.
---

# Reading context usage from disk

How to find how much context a Claude Code or Codex session is using, without
attaching to the session. Both agents write their token usage into their
transcript files. The numbers are exact — they come from the API response, not
an estimate.

Requires `jq`.

---

## Claude Code

**Transcripts:** `~/.claude/projects/<SLUG>/<SESSION-ID>.jsonl`

`<SLUG>` is the session's working directory with every non-alphanumeric
character replaced by `-`. Derive it:

```bash
echo "/Users/you/workspace/myproject" | sed 's/[^a-zA-Z0-9]/-/g'
```

**Context used** = the last assistant entry's
`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`.
Those three fields are disjoint and together are exactly the prompt that was
sent, so their sum is the context window occupancy at that moment.

### All sessions for a directory

```bash
DIR=/Users/you/workspace/myproject
cd ~/.claude/projects/$(echo "$DIR" | sed 's/[^a-zA-Z0-9]/-/g') && for f in *.jsonl; do jq -s --arg f "$f" '[.[] | select(.type=="assistant" and (.isSidechain|not) and .message.usage)] | last | if . then (.message.usage.input_tokens + .message.usage.cache_creation_input_tokens + .message.usage.cache_read_input_tokens) as $c | "\(($c/1000)|floor)k\t\(.message.model)\t\(.timestamp)\t\($f)" else empty end' "$f" -r; done | sort -rn
```

Output is `tokens`, `model`, `timestamp`, `filename`, largest first.

### One specific session file

```bash
jq -s '[.[] | select(.type=="assistant" and (.isSidechain|not) and .message.usage)] | last | .message.usage | .input_tokens + .cache_creation_input_tokens + .cache_read_input_tokens' FILE.jsonl
```

### The most recently active session in a directory

```bash
ls -t ~/.claude/projects/$(echo "$PWD" | sed 's/[^a-zA-Z0-9]/-/g')/*.jsonl | head -1
```

### Rules

- **Always filter `isSidechain`.** Subagent turns are written into the same
  file and have their own separate context. Without the filter you will often
  read a subagent's window and report it as the main session's.
- **The context limit is not in the file.** Only usage is recorded. Report the
  raw token count. Do not guess a limit and do not compute a percentage unless
  the user tells you the window size — limits vary by model and tier, and
  sessions above 200k tokens are normal on large-context models.
- **Compaction needs no special handling.** After a compact the next request is
  smaller, so reading the last entry reflects it automatically.

---

## Codex

**Transcripts:** `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`

Date-sharded by session start. Line 1 is a `session_meta` entry containing the
session's `cwd`.

**Context used** = the last `token_count` event's
`.payload.info.last_token_usage.input_tokens`.
**Context limit** = `.payload.info.model_context_window`, in the same event.

Codex records the limit, so a percentage is safe to compute here.

### All sessions for one day

```bash
for f in ~/.codex/sessions/2026/07/28/*.jsonl; do jq -s -r '(.[0].payload.cwd // "?") as $cwd | [.[] | select(.payload.type=="token_count") | .payload.info] | map(select(.!=null)) | last | select(.!=null) | .last_token_usage.input_tokens as $u | .model_context_window as $w | "\(($u/1000)|floor)k / \(($w/1000)|floor)k  \((100*$u/$w)|floor)%  \($cwd)"' "$f"; done | sort -rn
```

Change the date path to pick a different day. Use `~/.codex/sessions/*/*/*/*.jsonl`
for every session ever, but see the speed note below.

### One specific session file

```bash
jq -s '[.[] | select(.payload.type=="token_count") | .payload.info] | map(select(.!=null)) | last | {used: .last_token_usage.input_tokens, window: .model_context_window}' FILE.jsonl
```

### Find a session by working directory

Match `cwd` on line 1 only:

```bash
TARGET=/Users/you/workspace/myproject
for f in ~/.codex/sessions/*/*/*/*.jsonl; do head -1 "$f" | jq -r --arg f "$f" --arg t "$TARGET" 'select(.payload.cwd==$t) | $f'; done
```

### Rules

- **Use `last_token_usage`, never `total_token_usage`.** `total_` is the
  cumulative lifetime spend for the whole session and runs into the millions
  because it counts every cache read. Reporting it as "context used" is wrong
  by orders of magnitude.
- **Keep the `select(.!=null)` guards.** Short sessions emit no `token_count`
  event at all; without the guards `last` returns null and the expression
  errors out mid-sweep.
- **Do not use `grep -l <path>` to find a session by directory.** It matches
  the path anywhere in the file, so any session that merely read or mentioned
  that directory is a false positive. Match line 1 as shown above.
- **The full sweep is slow.** Scanning every session (`*/*/*/*.jsonl`) takes
  several seconds over a few thousand files. Restrict to a date directory when
  you can.
- **Percentages are approximate.** These are raw `input_tokens / window`.
  Codex's own display applies a baseline adjustment, so it will differ by a
  few points. Ordering and magnitude are correct.
- Archived sessions live separately under `~/.codex/archived_sessions/`.
  `~/.codex/session_index.jsonl` maps session `id` to `thread_name` if you want
  names instead of directories.

---

## Applies to both

- **File size on disk is not a proxy for context used.** Transcripts accumulate
  full history including content that has since been compacted away. A 1.8 MB
  transcript can be at a lower current context than a 1.6 MB one.
- **The reading is a snapshot, not live.** It reflects the last completed turn.
  A session that is mid-turn lags by one request.
- Both tools' subagents are visible: Claude Code interleaves them into the
  parent file (filter them out), Codex writes them as separate rollout files
  (they appear as their own rows).
