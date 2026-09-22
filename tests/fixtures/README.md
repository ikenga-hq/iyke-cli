# Test fixtures

- `ngwa-snapshot.golden.json` — verbatim copy of the shell's committed
  `NgwaSnapshot` golden (`shell/src/lib/ngwa/__fixtures__/ngwa-snapshot.golden.json`,
  shape-locked by WP-14/WP-17). `src/cmd/ngwa.rs`'s tests `include_str!` it to
  assert the `iyke ngwa` commands consume exactly the wire shape the shell
  renders. Refresh by re-copying the file; a drift in either direction should
  be a deliberate decision, not an accident.
