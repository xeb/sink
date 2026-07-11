# Failed Message Reason — Design

**Date:** 2026-07-10
**Status:** Approved

## Problem

When a message fails (e.g. "Yes, lock in the slider and add that to the Costco
order!"), the admin panel shows a red FAILED badge with no explanation. The
actual error (in that case, three BlueBubbles `500 Internal Server Error`
responses while sending Claude's reply) exists only in journalctl logs.
`db::mark_failed()` records no reason.

## Design

Store the failure reason and surface it in the admin panel.

1. **Schema** — add `error_reason TEXT` to the `messages` table via the
   existing best-effort `ALTER TABLE` migration pattern in `db.rs::init()`
   (same as `gemini_reason` / `attachments_json`).
2. **db.rs** — change `mark_failed(&self, guid)` to
   `mark_failed(&self, guid, reason: &str)`, writing `error_reason`. Add
   `error_reason` to the `Message` struct and all SELECTs.
3. **main.rs** — each of the 5 `mark_failed` call sites passes a descriptive
   reason built from the underlying error (command handler error, chat history
   failure, addressed-check DB failure, send failure, error-response send
   failure).
4. **web.rs** —
   - Include `error_reason` in `/api/messages` rows.
   - FAILED badges become clickable and open the existing modal component,
     showing the stored reason. Messages that failed before this change show
     "No error recorded (failed before error tracking was added) — check
     journalctl --user -u sink".
   - Bonus: `not_for_claude` badges open the same modal showing the
     already-stored `gemini_reason`.

## Rejected alternatives

- **Link FAILED to transcripts** — send failures occur after/outside Claude
  invocation and often have no transcript (true for the motivating case).
- **Parse journalctl at click time** — fragile, logs rotate, needs correlation
  heuristics.

## Testing

- `cargo build --release`, deploy, restart service.
- Verify migration added the column on the live DB.
- Verify old failed messages show the fallback text in the modal.
- Force-check UI renders by loading the Messages tab.
