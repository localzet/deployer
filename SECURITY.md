# Security Policy

## Reporting a Vulnerability

If you believe you have found a security vulnerability in Localzet Deployer, do not open a public issue.

Please report it privately to:

- Ivan Zorin
- Email: [creator@localzet.com](mailto:creator@localzet.com)

When possible, include:

- a clear description of the issue
- affected versions or commit range
- reproduction steps
- impact assessment
- suggested mitigation, if available

## Response Expectations

Best effort targets:

- initial acknowledgement within 7 days
- status update after triage
- coordinated disclosure after a fix is available

## Scope

Security reports are especially relevant for:

- authentication and authorization bypass
- webhook signature validation flaws
- secret leakage
- command execution and sandbox escape paths
- registry or deployment tampering
- log or history exposure across tenants or operators
