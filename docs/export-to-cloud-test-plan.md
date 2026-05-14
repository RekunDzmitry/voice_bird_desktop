# Export-to-Cloud: Manual Test Plan (Phases 5–7)

> Feature: `[e] key` uploads local transcript JSON to `POST /api/transcripts/upload`  
> Tests in this file are **manual** (not automated).  
> Automated unit + integration tests are in:
> - `voice_bird_desktop/src/app.rs` (`#[cfg(test)] mod tests`)
> - `voice_bird_web/src/app/api/transcripts/upload/__tests__/route.test.ts`

---

## 5. Manual E2E Test Scenarios

### 5.1 Setup

| Step | Action | Expected |
|------|--------|----------|
| M-01 | Ensure `config.toml` has a valid `voicebird_server_url` pointing at staging/production. | Config loads without error. |
| M-02 | Ensure `voicebird_api_key` is set to a valid key that exists in the target database. | Cloud toggle is available. |

### 5.2 Basic Workflow

| Step | Action | Expected |
|------|--------|----------|
| M-03 | Record a short session (~5 seconds of speech). Stop recording. | Session directory created with `transcript.json`, `meta.json`, `transcript.jsonl`, `transcript.txt`. |
| M-04 | Press `e` in idle mode. | Banner shows `"Exported ✓ — {uuid}"` in green. |
| M-05 | Check footer hint. | Footer shows `[e] export` in the idle hint row. |
| M-06 | Verify server: check DB — new row in `transcriptions` table. | Row has correct `userId`, `title`, `content`, `duration`, `metadata.session_id = dirname`. |
| M-07 | Verify Wasabi: S3 object exists at `transcripts/{userId}/{slug}.json`. | Downloadable, valid JSON. |
| M-08 | Press `e` again. | Banner shows `"Already exported ✓"` (no second HTTP call). |
| M-09 | Record a second session. Press `e`. | Only the new session is exported. |

### 5.3 Error Scenarios

| Step | Action | Expected |
|------|--------|----------|
| M-10 | Remove `voicebird_api_key` from config (or set to invalid), press `e`. | Red banner: `"Export failed: Server error: …"`. `.uploaded` marker NOT written. |
| M-11 | Set `voicebird_server_url` to an unreachable host, press `e`. | Red banner: `"Export failed: Network error: …"`. |
| M-12 | Delete or corrupt `transcript.json` in the session directory, press `e`. | Red banner: `"Failed to parse transcript: …"`. |
| M-13 | Delete all session directories, press `e`. | Banner: `"No sessions found to export"`. |

### 5.4 Race / Concurrency

| Step | Action | Expected |
|------|--------|----------|
| M-14 | Press `e` twice rapidly. | First press triggers upload. Second press may find `.uploaded` already written (returning `"Already exported ✓"`) or may race to also POST. Server-side idempotency catches the second POST — server returns `duplicate: true`. Final state is correct regardless. |

### 5.5 TUI Behavior

| Step | Action | Expected |
|------|--------|----------|
| M-15 | Start recording (any section active), press `e`. | Nothing happens — `e` is gated on `active_section_count() == 0`. |
| M-16 | Open path modal (`p`), press `e`. | `e` is handled by modal text-input handlers (either inserts `e` or ignored — acceptable either way since modal already has text focus). |
| M-17 | Banner clears correctly — press `e`, see export result, then start a new recording. | Export banner disappears (replaced by recording UI). |
| M-18 | Banner clears correctly — press `e`, see result, then press `x` to clear transcript. | Export banner remains (it's independent of transcript clear). |

### 5.6 Layout Regression

| Step | Action | Expected |
|------|--------|----------|
| M-19 | Export banner row appears above footer without shrinking transcript area. | Layout has correct number of rows. |
| M-20 | Both error banner (`! msg` in red) and export banner appear simultaneously. | Both rows visible, no overlap. Error banner first, then export banner. |
| M-21 | Footer line still fits in 80-column terminal after adding `[e] export`. | No line wrapping at 80 cols. |

---

## 6. Performance / Edge Cases

| ID | Scenario | Concern | Mitigation / Acceptance |
|----|----------|---------|-------------------------|
| PF-01 | Session directory with 10,000 subdirectories | `find_latest_session` reads all entries, sorts them | Acceptable — 10k readdir + sort is <50ms on modern SSD. Cap at 500 entries later if needed. |
| PF-02 | Transcript with 5,000 segments (2-hour meeting) | JSON serialization + HTTP body size | `ureq` sends chunked. Wasabi accepts up to 5TB objects. Add `bodySizeLimit` in Next.js config if needed. |
| PF-03 | Serverless timeout (Vercel hobby: 10s) | Upload + S3 + DB insert may exceed 10s | Warn in docs; recommend Pro plan or self-host for large transcripts. |

---

## 7. Execution Order for Manual Phase

| Order | Steps | Notes |
|-------|-------|-------|
| 1 | M-01, M-02 | Setup — ensure config is valid |
| 2 | M-03 through M-09 | Happy path and idempotency |
| 3 | M-10 through M-13 | Error scenarios |
| 4 | M-14 | Race condition |
| 5 | M-15 through M-21 | TUI + layout regression |
| 6 | PF-01 through PF-03 | Performance review (documentation, no code changes expected) |
