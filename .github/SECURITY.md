# Security Policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private vulnerability
reporting for this repository when available, or contact the repository owner privately through
their GitHub profile. Include the affected version, operating system, impact, reproduction steps
and any suggested mitigation.

The project will acknowledge a report, investigate it, and coordinate a fix and disclosure as
capacity permits. No response-time or support SLA is currently offered.

## Scope

canfit reads process and memory information and can, after explicit confirmation, ask the operating
system to close processes. Issues that are especially relevant:

- closing a process without confirmation, or suggesting a protected or system process;
- running with more privileges than needed, or escalating privileges;
- reading or writing data outside canfit's own local data directory;
- any network traffic the user did not explicitly request.

## Supported versions

canfit has no releases yet. Once releases exist, security fixes target the latest release and the
default branch.
