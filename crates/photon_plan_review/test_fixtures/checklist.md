# Move search to the new index

Some context about why.

## Build the index

- [ ] Add `IndexBuilder` in `src/index/builder.rs`
- [ ] Run `cargo test index` until it passes

## Switch readers over

- [ ] Point `SearchService::query` at the new index
- [ ] Delete `legacy_lookup`

## Notes

No checklist here, so this is not a task.
