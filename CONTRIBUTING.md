# Contributing

Thank you for contributing to Localzet Deployer.

## Before You Start

- Read the README to understand the current architecture and operating model.
- Open an issue for larger changes before starting implementation.
- Keep pull requests focused and easy to review.

## Development Guidelines

- Use English for all public-facing text and documentation.
- Keep code comments short, technical, and only where needed.
- Preserve the existing structure unless a refactor is clearly justified.
- Prefer small, explicit changes over broad rewrites.

## Pull Request Expectations

Please include:

- a clear summary of the change
- the reason for the change
- any configuration, migration, or operational impact
- test or verification notes

## Areas That Need Extra Care

- webhook validation
- authentication and authorization
- shell execution and command templating
- deployment and rollback behavior
- database schema changes

## Security

Do not open public issues for sensitive vulnerabilities. Use the process described in [SECURITY.md](./SECURITY.md).
