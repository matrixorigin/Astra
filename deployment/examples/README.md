# Deployment examples

Astra maintains deployment assets for its supported runtime profiles instead
of provider-specific templates that cannot encode an organization's network,
identity, secret-management, and database requirements safely.

Choose the supported baseline closest to your environment:

- [Single-host Docker Compose](../all-in-one/README.md) for local evaluation or
  a compact server deployment.
- [Kubernetes and Helm](../kubernetes/README.md) for a server-only cluster
  deployment.
- [Production deployment](../../docs/quickstart/production.md) for required
  secrets, readiness checks, and Server + User Runner topology.

Start with the [deployment overview](../README.md) and
[configuration reference](../../docs/reference/configuration.md). Adapt the
container or Helm baseline in your own infrastructure repository, where cloud
accounts, private endpoints, IAM policies, TLS, and secret references can be
reviewed under your organization's controls.

The former AWS, GCP, and systemd examples were removed because they referenced
missing assets and obsolete runtime commands. They were not deployable
templates for the current Rust implementation.
