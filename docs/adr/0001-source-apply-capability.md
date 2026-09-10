# ADR 0001: Source-apply capability is not adopted in this line

- Status: accepted
- Date: 2026-09-10
- Related: #18 (epic), #19 (changeset), #20 (proposed `change(action=apply|recover)`)

## Context

The `agz-rust-coder` product guarantee is a bounded, source-write-free Rust
correctness MCP server. Issue #20 proposed an opt-in capability that would write
validated candidate changes into the operator workspace, with per-file safe
replace, a durable journal, recovery, mutation leases, and cross-platform
fault-injection evidence.

The epic's first product already delivers a verified revision-bound patch
package (`change(action=export)`) plus compiler diagnostics and repair
candidates. Applying that package is a host/operator action with its own
version control and review workflow. A server-side apply path would add
multi-file non-atomic writes, conflict detection against concurrent editors,
rollback/recovery semantics, and platform-specific fault-injection guarantees
that are not justified by a concrete operator requirement, while widening the
security surface that the write-free guarantee exists to prevent.

## Decision

This line does not adopt `change(action=apply|recover)` and keeps every current
tool source-write-free. Validated deliverables leave the server as bounded
export packages; application remains in the host or operator workflow. A future
source-apply capability requires a separate accepted ADR that defines the
allowlisted roots and operations, journal/recovery contract, lease semantics,
fault-injection evidence, and the fail-closed default.

## Consequences

- Issue #20 is closed as not planned for this line; it can be reopened with a
  concrete operator requirement and the dedicated ADR above.
- Existing guarantees and tests remain authoritative: original workspace hashes
  are asserted unchanged after `change`, `repair`, `api`, and `work` operations.
- Delivery stays reviewable: `change(action=export)` carries the revision,
  hashes, patches and validation evidence; the host decides where and how to
  apply it.

## Türkçe özet

Bu hatta kaynak dosyaya yazma yeteneği (önerilen `change(action=apply|recover)`)
benimsenmedi; ürünün write-free teslim garantisi korunuyor. Doğrulanmış değişiklik
`change(action=export)` paketiyle host'a/operatöre teslim edilir. Gelecekte bir
kaynak-uygulama yeteneği ancak ayrı bir kabul edilmiş ADR ile (izinli kökler,
günlük/kurtarma, kira, hata enjeksiyonu kanıtı ve varsayılan fail-closed) açılabilir.
