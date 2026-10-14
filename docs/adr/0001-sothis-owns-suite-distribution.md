# ADR 0001: Sothis owns suite distribution

- Status: accepted
- Date: 2026-08-25
- Ametrite decision: D-2

## Context

The suite marketplace and one-shot installer originally lived in Sirius
Forester. That made one member repository responsible for distributing other
independent tools and created multiple valid plugin identities.

## Decision

`Davidb3l/Sothis` owns the canonical marketplace, suite manifest, installation,
update, compatibility checks, shared event schemas, and suite-level doctor.

Sirius Forester continues to own the runtime loop:

`claim -> lock -> brief -> execute -> gate -> receipt`.

The canonical plugin identities use the `@sothis` marketplace. The existing
Sirius Forester marketplace remains available as a compatibility path for two
Sothis releases, then may be removed through an explicit migration release.

## Consequences

- Sothis composes tools but does not write their stores or absorb their domain
  logic.
- Every member remains independently installable.
- Suite facts are versioned in `suite.manifest.json` and consumed by the CLI.
- Documentation and website data must be validated against the manifest.
- Legacy plugin identities are detectable migration inputs, not canonical
  names on new Sothis surfaces.

The cross-tool contracts this ownership implies (version handshake, doctor
envelope, Windows spawn rules, peer resolution, schema numbers, plugin
identities) are specified in [`docs/CONTRACTS.md`](../CONTRACTS.md).
