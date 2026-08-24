-- M12.5 groundwork: what the classifier said, not only what the router used.
--
-- `task` alone cannot be read back. A row saying `chat` means either "the heuristic was certain"
-- or "it guessed something at 0.2 and docs/12's confidence gate fell back to chat" — opposite
-- facts about the same field. Both were discarded at the call site
-- (`let (task, _confidence, _trusted) = ...`), so every request threw away the one signal a
-- learned router (M19.3) would be trained on, and no amount of later analysis could recover it.
--
-- tenant-scoping: inherited. These are columns on `route_decisions`, which is already scoped by
-- `account_id` and indexed on it; nothing here changes what a query must filter by.
--
-- Nullable, because rows written before this migration genuinely have no value. A DEFAULT would
-- put a number on history that was never measured, which is worse than a gap: a gap is visibly a
-- gap, and 0.0 reads as "the classifier was certain of nothing" rather than "nobody asked".
ALTER TABLE route_decisions ADD COLUMN IF NOT EXISTS confidence real;
ALTER TABLE route_decisions ADD COLUMN IF NOT EXISTS trusted boolean;
