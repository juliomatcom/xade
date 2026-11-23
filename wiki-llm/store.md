# Store
- DB is `dirs::data_dir()/agent-workspace/workspace.db`; the desktop app picks the path and opens it (`aw-store` takes a path).
- One writer: `StoreWriter` owns the only write connection on a std thread; every `WriteOp` is its own transaction, failures are logged and never fatal; `WriterHandle::flush` waits for pending ops.
- Readers (`StoreReader`) use their own query-only connection behind a mutex via `spawn_blocking`; WAL lets them run beside the writer.
- Migrations are keyed by `PRAGMA user_version`; the writer applies them on open, and a newer DB than the code is an error.
- FTS5 trigram `search_index` is filled only by triggers: session titles, and message text for user/assistant roles.
- Search: empty lists; under 3 chars is `LIKE` on title and preview; otherwise FTS plus a nucleo title fuzzy pass. Rank: exact title substring, FTS bm25, fuzzy-only score; ties by `updated_at` newest.
- Pages are zero-based; `max_seq` is 0 for a session with no messages. Timestamps are fixed-width UTC RFC3339 text.
- Deltas and session status are never stored; there is no attachments table yet.
