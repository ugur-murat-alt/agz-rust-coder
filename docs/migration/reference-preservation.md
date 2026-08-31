# Reference repository preservation ledger

The TypeScript repository at `/home/ugur/Projects/opencode-rust-coder` is a
read-only migration reference. This ledger was captured before Rust product
implementation on 2026-08-31.

- Branch: `main`
- HEAD: `0672c255dd7e7098dda54f04d9ac001cf164e199`
- `git status --porcelain=v1 -z` SHA-256:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
- `git diff --binary` SHA-256:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
- `git ls-files --others --exclude-standard -z` SHA-256:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`

The same commands and HEAD check must be repeated after final indexing. A
different value is concurrent external activity or an accidental write and
must not be silently reverted or attributed without evidence.
